//! Tests for the "Retry Failed Metadata" pass.
//!
//! The first three tests are deterministic and need no network. The large
//! playlist test is `#[ignore]`d and requires network + yt-dlp:
//!     cargo test --test metadata_retry -- --ignored --nocapture

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use soundcloud2mp4::config::settings::Settings;
use soundcloud2mp4::downloader::ytdlp::{self, FlatEntry};
use soundcloud2mp4::models::messages::{Tx, WorkerMsg};
use soundcloud2mp4::models::track::{MetaFailure, TrackMetadata};
use soundcloud2mp4::pipeline;
use tokio_util::sync::CancellationToken;

/// A large playlist (200+ tracks) for the network tests. Provide your own via
/// the `SC2MP4_TEST_PLAYLIST` environment variable — for example an artist's
/// `/tracks` page, which is a reliable way to get a stable, large entry list.
/// The default is a placeholder that will not resolve, so these `#[ignore]`d
/// tests only run meaningfully once you point them at a real playlist you have
/// the rights to use.
fn big_playlist() -> String {
    std::env::var("SC2MP4_TEST_PLAYLIST")
        .unwrap_or_else(|_| "https://soundcloud.com/xxxxxxxxxxxxxxxx".to_string())
}

fn channel() -> (Tx, Receiver<WorkerMsg>) {
    let (tx_raw, rx) = std::sync::mpsc::channel();
    (Tx { tx: tx_raw, ctx: egui::Context::default() }, rx)
}

/// Everything the GUI would have learned from a run.
#[derive(Default)]
struct Collected {
    meta: HashMap<usize, Result<TrackMetadata, MetaFailure>>,
    retrying: Vec<usize>,
    progress: Vec<(usize, usize)>,
    /// (recovered, still_failed, cancelled, gave_up)
    finished: Option<(usize, usize, bool, bool)>,
    /// Tracks the last finished retry pass judged permanently unavailable.
    finished_permanent: usize,
    logs: Vec<String>,
    /// (cancelled, failed, rate_limited)
    load_finished: Option<(bool, usize, usize)>,
    /// Tracks the initial load judged permanently unavailable.
    load_permanent: usize,
    /// Tracks the initial load judged access-restricted (recoverable).
    load_restricted: usize,
    /// Tracks the last retry pass judged access-restricted.
    finished_restricted: usize,
}

impl Collected {
    /// Rows the manual retry button would still offer.
    fn retryable_failures(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .meta
            .iter()
            .filter(|(_, r)| matches!(r, Err(e) if !e.is_permanent()))
            .map(|(i, _)| *i)
            .collect();
        v.sort_unstable();
        v
    }

    /// Rows SoundCloud refused anonymously — not confirmed unavailable.
    fn restricted_failures(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .meta
            .iter()
            .filter(|(_, r)| matches!(r, Err(e) if e.is_restricted()))
            .map(|(i, _)| *i)
            .collect();
        v.sort_unstable();
        v
    }

    /// Rows the GUI would mark unavailable and stop retrying.
    fn permanent_failures(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .meta
            .iter()
            .filter(|(_, r)| matches!(r, Err(e) if e.is_permanent()))
            .map(|(i, _)| *i)
            .collect();
        v.sort_unstable();
        v
    }
}

fn collect(rx: &Receiver<WorkerMsg>) -> Collected {
    let mut c = Collected::default();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            WorkerMsg::TrackMeta(i, r) => {
                c.meta.insert(i, r);
            }
            WorkerMsg::MetaRetrying(i) => c.retrying.push(i),
            WorkerMsg::MetaRetryProgress { remaining, total } => c.progress.push((remaining, total)),
            WorkerMsg::MetaRetryFinished {
                recovered,
                still_failed,
                permanent,
                restricted,
                cancelled,
                gave_up,
            } => {
                c.finished = Some((recovered, still_failed, cancelled, gave_up));
                c.finished_permanent = permanent;
                c.finished_restricted = restricted;
            }
            WorkerMsg::MetaLoadFinished {
                cancelled,
                failed,
                rate_limited,
                permanent,
                restricted,
            } => {
                c.load_finished = Some((cancelled, failed, rate_limited));
                c.load_permanent = permanent;
                c.load_restricted = restricted;
            }
            WorkerMsg::Log(l) => c.logs.push(l),
            _ => {}
        }
    }
    c
}

fn entry(id: &str) -> FlatEntry {
    FlatEntry {
        id: id.into(),
        url: format!("https://soundcloud.com/test/{id}"),
        title: None,
    }
}

/// Settings that never reach the network: yt-dlp path points at nothing.
/// Attempts and the back-off are kept small so the test exercises the *logic*
/// without sleeping through the real 5s→60s ladder.
fn offline_settings() -> Settings {
    Settings {
        ytdlp_path: "definitely-not-a-real-yt-dlp-binary".into(),
        debug_mode: false,
        retry_delay_ms: 250, // the configured floor, to keep the test quick
        // Full concurrency so consecutive penalties accumulate quickly and the
        // 1s-floor back-off tests finish fast.
        retry_concurrency: 4,
        metadata_retry_max_attempts: 3,
        retry_initial_delay_secs: 1, // the floor; the ladder is [1s, 1s, 1s]
        retry_max_delay_secs: 1,
        ..Settings::default()
    }
}

#[test]
fn retry_reports_progress_and_logs_every_attempt() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (tx, rx) = channel();
        let targets = vec![(3, entry("aaa")), (7, entry("bbb"))];

        pipeline::retry_failed_metadata(
            offline_settings(),
            String::new(),
            targets,
            tx,
            CancellationToken::new(),
        )
        .await;

        let c = collect(&rx);

        // Only the two requested rows were touched.
        assert_eq!(c.retrying.len(), 2);
        assert!(c.retrying.contains(&3) && c.retrying.contains(&7));
        let touched: Vec<usize> = c.meta.keys().copied().collect();
        assert_eq!(touched.len(), 2, "retry must not touch other rows: {touched:?}");
        assert!(c.meta[&3].is_err() && c.meta[&7].is_err());

        // Progress counts down from total to 0.
        assert_eq!(c.progress.first(), Some(&(2usize, 2usize)));
        assert_eq!(c.progress.last(), Some(&(0usize, 2usize)));

        assert_eq!(c.finished, Some((0, 2, false, false)));

        // Every attempt is logged with the track id and attempt number.
        let attempts: Vec<&String> = c
            .logs
            .iter()
            .filter(|l| l.contains("transient error on track"))
            .collect();
        assert!(
            attempts.iter().any(|l| l.contains("aaa") && l.contains("attempt 1/3")),
            "missing attempt log: {attempts:?}"
        );
        assert!(
            attempts.iter().any(|l| l.contains("attempt 3/3")),
            "should exhaust all 3 attempts: {attempts:?}"
        );
        assert!(
            c.logs.iter().any(|l| l.contains("global cooldown")),
            "transient failures should trigger the shared cooldown: {:?}",
            c.logs
        );
    });
}

#[test]
fn cancelled_retry_stops_promptly() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (tx, rx) = channel();
        let targets: Vec<_> = (0..50).map(|i| (i, entry(&format!("t{i}")))).collect();
        let token = CancellationToken::new();

        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            cancel.cancel();
        });

        let started = Instant::now();
        pipeline::retry_failed_metadata(offline_settings(), String::new(), targets, tx, token).await;
        let elapsed = started.elapsed();

        // Without cancellation this would take 50 tracks x 3 attempts.
        assert!(elapsed.as_secs() < 5, "cancellation was not prompt: {elapsed:?}");

        let c = collect(&rx);
        let (_, _, cancelled, _) =
            c.finished.expect("must report completion even when cancelled");
        assert!(cancelled, "finish message must say it was cancelled");
        assert!(
            c.meta.len() < 50,
            "a cancelled pass should not have resolved every track"
        );
    });
}

#[test]
fn empty_target_list_finishes_immediately() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (tx, rx) = channel();
        pipeline::retry_failed_metadata(
            offline_settings(),
            String::new(),
            Vec::new(),
            tx,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(collect(&rx).finished, Some((0, 0, false, false)));
    });
}

#[test]
fn retry_pacing_is_never_more_aggressive_than_the_initial_load() {
    // Even a hand-edited config cannot exceed the initial load's 4 workers
    // or drop below the minimum delay.
    let s = Settings { retry_concurrency: 99, retry_delay_ms: 0, ..Settings::default() };
    assert_eq!(s.retry_workers(), 4);
    assert_eq!(s.retry_delay().as_millis(), 250);

    let d = Settings::default();
    assert_eq!(d.retry_workers(), 2);
    assert_eq!(d.retry_delay().as_millis(), 500);

    // The rate-limit ladder defaults to the exponential 5s→10s→20s→40s→60s→60s.
    let steps: Vec<u64> = d.retry_backoff().iter().map(|x| x.as_secs()).collect();
    assert_eq!(steps, vec![5, 10, 20, 40, 60, 60]);

    // The ladder always has one rung per attempt and is never empty.
    assert_eq!(d.retry_backoff().len(), d.metadata_retry_attempts() as usize);
    let clamped = Settings {
        metadata_retry_max_attempts: 0,
        ..Settings::default()
    };
    assert!(!clamped.retry_backoff().is_empty());
}

/// A hard rate limit must not turn into an unbounded grind: once the ladder
/// tops out with no successes, the pass stops early and hands over to the
/// manual button.
#[test]
fn persistent_rate_limiting_stops_the_pass_early() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (tx, rx) = channel();
        // Far more tracks than the abort guard allows attempts for.
        let targets: Vec<_> = (0..80).map(|i| (i, entry(&format!("t{i}")))).collect();

        let started = Instant::now();
        pipeline::retry_failed_metadata(
            offline_settings(),
            String::new(),
            targets,
            tx,
            CancellationToken::new(),
        )
        .await;
        let elapsed = started.elapsed();

        let c = collect(&rx);
        let (recovered, still_failed, cancelled, gave_up) = c.finished.expect("must finish");
        assert_eq!(recovered, 0);
        assert_eq!(still_failed, 80);
        assert!(!cancelled, "giving up is not the same as user cancellation");
        assert!(gave_up, "should have stopped early: took {elapsed:?}");
        assert!(
            c.logs.iter().any(|l| l.contains("giving up on this batch")),
            "the abort should be logged"
        );
        // Stopping early means most tracks were never attempted.
        assert!(
            c.meta.len() < 80,
            "abort should skip remaining tracks, resolved {}",
            c.meta.len()
        );
    });
}

/// Regression: on a large playlist a handful of tracks never recovered no
/// matter how many times "Retry Failed Metadata" was pressed.
///
/// Errors that really do mean the track is gone.
#[test]
fn dead_tracks_are_classified_permanently_unavailable_with_a_reason() {
    let cases = [
        (
            "ERROR: [soundcloud] 1234567890: Unable to download JSON metadata: \
             HTTP Error 404: Not Found (caused by <HTTPError 404: Not Found>)",
            "Track no longer exists",
        ),
        (
            "ERROR: [soundcloud] 123: Track is private and cannot be accessed",
            "Track is private",
        ),
        (
            "ERROR: This track has been removed by the uploader",
            "Track was removed by the uploader",
        ),
        (
            "ERROR: Unsupported URL: https://soundcloud.com/x",
            "Not a supported SoundCloud page",
        ),
        (
            "ERROR: [soundcloud] 9: This track is unavailable in your country",
            "Track is not available in your country",
        ),
    ];
    for (stderr, expected) in cases {
        assert_eq!(
            ytdlp::permanent_failure_reason(stderr),
            Some(expected),
            "wrong reason for: {stderr}"
        );
        assert!(!ytdlp::is_transient_failure(stderr), "should not be retried: {stderr}");
    }
}

/// `This video is DRM protected` must **not** be treated as permanent.
///
/// It reads like a verdict but is yt-dlp's fallback after the plain-MP3
/// formats fail to resolve. The pattern seen on affected tracks:
///
/// ```text
/// [soundcloud] 1234567890: Downloading info JSON             <- succeeds
/// WARNING: [soundcloud] 1234567890: hls_mp3 format not found
/// WARNING: [soundcloud] 1234567890: http_mp3 format not found
/// ERROR:   [soundcloud] 1234567890: This video is DRM protected
/// ```
///
/// Asking `api-v2.soundcloud.com` about those same ids returns `policy: ALLOW`,
/// `streamable: true`, `snipped: false`, a title and a full duration — plus an
/// unencrypted `mp3_1_0` progressive transcoding that 404s only when resolved
/// anonymously. The audio is not encrypted; access is being withheld. Calling
/// that "DRM protected and cannot be downloaded" told the user a recoverable
/// track was dead.
#[test]
fn drm_message_is_restricted_access_not_a_permanent_verdict() {
    for stderr in [
        "ERROR: [soundcloud] 1234567890: This video is DRM protected",
        "ERROR: [soundcloud] 1234567891: This video is DRM-protected",
    ] {
        let (kind, reason) = ytdlp::classify_failure(stderr);
        assert_eq!(kind, ytdlp::FailureKind::Restricted, "{stderr}");
        assert_eq!(reason, Some(ytdlp::RESTRICTED_REASON));
        // Crucially: not permanent, so nothing declares the track unrecoverable.
        assert_eq!(ytdlp::permanent_failure_reason(stderr), None, "{stderr}");
    }

    // The wording states what was observed rather than asserting a cause, and
    // never claims the track cannot be downloaded.
    let reason = ytdlp::RESTRICTED_REASON.to_lowercase();
    assert!(reason.contains("restricted access"), "{reason}");
    assert!(reason.contains("authentication") && reason.contains("extractor"), "{reason}");
    assert!(!reason.contains("drm"), "must not repeat the DRM claim: {reason}");
    assert!(!reason.contains("cannot"), "must not assert impossibility: {reason}");
}

/// The browser dropdown must reach every yt-dlp call, and a manually set file
/// must win over it (it is only ever set after a browser has failed).
#[test]
fn cookie_source_resolves_from_the_browser_setting() {
    use soundcloud2mp4::config::settings::CookieBrowser;
    use soundcloud2mp4::downloader::cookies::CookieSource;

    let anon = Settings { cookie_browser: CookieBrowser::None, ..Settings::default() };
    assert_eq!(anon.cookie_source(), CookieSource::None);
    assert!(anon.cookie_source().is_none());

    for browser in [CookieBrowser::Chrome, CookieBrowser::Edge, CookieBrowser::Firefox] {
        let s = Settings { cookie_browser: browser, ..Settings::default() };
        assert_eq!(s.cookie_source(), CookieSource::Browser(browser));
        assert_eq!(
            s.cookie_source().describe(),
            format!("--cookies-from-browser {}", browser.ytdlp_name().unwrap())
        );
    }

    // A path that does not exist is ignored rather than fatal — yt-dlp aborts
    // on a bad --cookies, which would break every load.
    let stale = Settings {
        cookie_browser: CookieBrowser::Firefox,
        cookies_path: "C:\\nope\\missing-cookies.txt".into(),
        ..Settings::default()
    };
    assert!(stale.cookies_missing());
    assert_eq!(
        stale.cookie_source(),
        CookieSource::Browser(CookieBrowser::Firefox),
        "a missing file must fall back to the browser, not disable cookies"
    );
}

/// A leftover cookies file must never quietly override a browser the user just
/// picked — and a JSON export (the usual mistake) must be rejected rather than
/// handed to yt-dlp, which aborts on it and would break every request.
#[test]
fn a_stale_or_json_cookies_file_cannot_hijack_the_browser_choice() {
    use soundcloud2mp4::config::settings::{looks_like_netscape_cookies, CookieBrowser};
    use soundcloud2mp4::downloader::cookies::CookieSource;

    // What a browser extension typically exports, and what yt-dlp wants.
    assert!(!looks_like_netscape_cookies(
        r#"[{"domain":".soundcloud.com","name":"oauth_token","value":"x"}]"#
    ));
    assert!(!looks_like_netscape_cookies("{\"cookies\": []}"));
    assert!(!looks_like_netscape_cookies("some~opaque~token~blob"));
    assert!(looks_like_netscape_cookies(
        "# Netscape HTTP Cookie File\n.soundcloud.com\tTRUE\t/\tTRUE\t0\tk\tv\n"
    ));
    // Header-less but structurally valid is still accepted.
    assert!(looks_like_netscape_cookies(
        ".soundcloud.com\tTRUE\t/\tTRUE\t0\tk\tv\n"
    ));

    let dir = std::env::temp_dir().join(format!("sc2mp4_cookiefmt_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let json = dir.join("json_export.txt");
    std::fs::write(&json, r#"[{"name":"oauth_token","value":"x"}]"#).unwrap();

    // Browser selected + bad file present: the browser must win.
    let with_browser = Settings {
        cookie_browser: CookieBrowser::Firefox,
        cookies_path: json.to_string_lossy().into_owned(),
        ..Settings::default()
    };
    assert_eq!(
        with_browser.cookie_source(),
        CookieSource::Browser(CookieBrowser::Firefox),
        "a leftover file must not override an explicit browser choice"
    );

    // No browser + bad file: run anonymously rather than feed yt-dlp garbage.
    let file_only = Settings {
        cookie_browser: CookieBrowser::None,
        cookies_path: json.to_string_lossy().into_owned(),
        ..Settings::default()
    };
    assert!(file_only.cookies_wrong_format());
    assert!(!file_only.cookies_missing(), "it exists, it is just the wrong format");
    assert_eq!(
        file_only.cookie_source(),
        CookieSource::None,
        "a JSON export must be ignored, not passed to --cookies"
    );

    // A genuine Netscape file is used.
    let good = dir.join("netscape.txt");
    std::fs::write(&good, "# Netscape HTTP Cookie File\n.soundcloud.com\tTRUE\t/\tTRUE\t0\tk\tv\n")
        .unwrap();
    let valid = Settings {
        cookie_browser: CookieBrowser::None,
        cookies_path: good.to_string_lossy().into_owned(),
        ..Settings::default()
    };
    assert!(!valid.cookies_wrong_format());
    assert_eq!(valid.cookie_source(), CookieSource::File(good));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Metadata for these tracks now resolves, so they appear as ordinary
/// selectable rows — but the audio is still withheld anonymously and the
/// failure resurfaces at download time. That message must be equally accurate.
#[test]
fn restricted_download_failures_point_at_cookies_not_drm() {
    let raw = "yt-dlp exited with code 1\n\nstderr:\n\
               ERROR: [soundcloud] 1234567890: This video is DRM protected";
    let annotated = ytdlp::annotate_download_failure(raw);
    assert!(annotated.starts_with(raw), "the original error must be preserved");
    assert!(
        annotated.contains("SoundCloud Authentication"),
        "should name the setting: {annotated}"
    );
    assert!(
        annotated.contains("may be fine") || annotated.contains("withholding"),
        "should not imply the track is gone: {annotated}"
    );

    // An ordinary failure is passed through untouched.
    let other = "yt-dlp exited with code 1\n\nstderr:\nERROR: HTTP Error 503";
    assert_eq!(ytdlp::annotate_download_failure(other), other);
}

/// A restricted track is skipped by the automatic pass (an identical anonymous
/// request would fail identically) but still offered by the manual button,
/// since the user may have supplied cookies in the meantime.
#[test]
fn restricted_tracks_are_manually_retryable_but_not_auto_retried() {
    let restricted = MetaFailure::restricted("drm", ytdlp::RESTRICTED_REASON);
    assert!(restricted.is_restricted());
    assert!(!restricted.is_permanent(), "must not be reported as unavailable");
    assert!(!restricted.is_auto_retryable(), "automatic pass would just re-fail");

    let throttled = MetaFailure::transient("403");
    assert!(throttled.is_auto_retryable());

    let dead = MetaFailure::permanent("404", "Track no longer exists");
    assert!(dead.is_permanent());
    assert!(!dead.is_auto_retryable());

    // What each button acts on, mirroring the GUI's filters.
    let rows = [&restricted, &throttled, &dead];
    let auto: Vec<_> = rows.iter().filter(|f| f.is_auto_retryable()).collect();
    let manual: Vec<_> = rows.iter().filter(|f| !f.is_permanent()).collect();
    assert_eq!(auto.len(), 1, "only the throttled row is auto-retried");
    assert_eq!(manual.len(), 2, "manual retry also offers the restricted row");
}

/// The trap this classifier was rebuilt around: SoundCloud reports throttling
/// as 403, never 429. Treating 403 as permanent would mark hundreds of healthy
/// tracks unavailable and disable the back-off entirely.
#[test]
fn throttling_is_never_mistaken_for_a_dead_track() {
    for stderr in [
        "ERROR: [soundcloud:user] p: Unable to download JSON metadata: \
         HTTP Error 403: Forbidden (caused by <HTTPError 403: Forbidden>)",
        "ERROR: 'NoneType' object is not subscriptable",
        "ERROR: unable to download API page: HTTP Error 429: Too Many Requests",
        "ERROR: Unable to download webpage: HTTP Error 503: Service Unavailable",
        "ERROR: something nobody has seen before",
    ] {
        assert_eq!(
            ytdlp::permanent_failure_reason(stderr),
            None,
            "throttling must stay retryable: {stderr}"
        );
    }
}

/// A permanent failure must be excluded from the retry work list, which is
/// what stops the "press retry forever" loop. Mirrors the GUI's filter.
#[test]
fn permanent_failures_are_excluded_from_the_retry_work_list() {
    let rows = [
        Err(MetaFailure::permanent("404", "Track no longer exists")),
        Err(MetaFailure::transient("HTTP Error 403: Forbidden")),
        Err(MetaFailure::permanent("private", "Track is private")),
        Ok(()),
    ];
    let retryable = rows
        .iter()
        .filter(|r| matches!(r, Err(e) if !e.is_permanent()))
        .count();
    let permanent = rows
        .iter()
        .filter(|r| matches!(r, Err(e) if e.is_permanent()))
        .count();
    assert_eq!(retryable, 1, "only the throttled row may be retried");
    assert_eq!(permanent, 2);

    // The row text names the cause rather than a yt-dlp stack trace.
    let dead = MetaFailure::permanent(
        "yt-dlp failed (metadata fetch):\nERROR: HTTP Error 404: Not Found",
        "Track no longer exists",
    );
    assert_eq!(dead.headline(), "Track no longer exists");
    // A retryable failure still shows the raw first line.
    let throttled = MetaFailure::transient("HTTP Error 403: Forbidden\nsecond line");
    assert_eq!(throttled.headline(), "HTTP Error 403: Forbidden");
}

// --------------------------------------------------------------- network ----

/// Diagnostic: what do real failures actually look like, and how does
/// `is_transient_failure` classify them? Prints a histogram rather than
/// asserting, so the classifier can be tuned against measured reality.
#[test]
#[ignore = "requires network and yt-dlp; diagnostic"]
fn classify_real_failures() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let settings = Settings::load();
        let (tx, _rx) = channel();
        let token = CancellationToken::new();

        let playlist = big_playlist();
        let (_, entries) =
            ytdlp::fetch_playlist(&settings.ytdlp_path, &playlist, &settings.cookie_source(), &token, &tx, None)
                .await
                .expect("playlist should load");

        // Enough parallelism to provoke the rate limit quickly.
        let mut set = tokio::task::JoinSet::new();
        for entry in entries.into_iter().take(60) {
            let path = settings.ytdlp_path.clone();
            let tx = tx.clone();
            let token = token.clone();
            set.spawn(async move {
                ytdlp::fetch_track_info_detailed(
                    &path,
                    &entry,
                    &soundcloud2mp4::downloader::cookies::CookieSource::None,
                    &token, &tx, None)
                    .await
                    .err()
            });
        }

        let mut histogram: HashMap<String, usize> = HashMap::new();
        let mut ok = 0usize;
        let (mut transient, mut permanent) = (0usize, 0usize);
        while let Some(res) = set.join_next().await {
            match res.unwrap() {
                None => ok += 1,
                Some(e) => {
                    if e.transient() {
                        transient += 1
                    } else {
                        permanent += 1
                    }
                    let key = format!(
                        "exit={} kind={:?} reason={} | {}",
                        e.code_str(),
                        e.kind,
                        e.reason.unwrap_or("(none)"),
                        e.message.lines().take(2).collect::<Vec<_>>().join(" / ")
                    );
                    *histogram.entry(key).or_default() += 1;
                }
            }
        }

        println!("\n=== {ok} ok, {transient} transient, {permanent} permanent ===");
        let mut rows: Vec<_> = histogram.into_iter().collect();
        rows.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (msg, n) in rows.iter().take(10) {
            println!("[{n}x] {msg}");
        }
    });
}

/// Initial load of a 200+ track playlist, then a retry of whatever failed.
/// Asserts the two properties that matter: successful rows are untouched, and
/// the retry only ever improves the outcome.
#[test]
#[ignore = "requires network and yt-dlp; takes several minutes"]
fn large_playlist_load_then_retry_failed() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let settings = Settings::load();
        let (tx, rx) = channel();
        let token = CancellationToken::new();

        let playlist = big_playlist();
        let (title, entries) =
            ytdlp::fetch_playlist(&settings.ytdlp_path, &playlist, &settings.cookie_source(), &token, &tx, None)
                .await
                .expect("big playlist should load");
        println!("playlist '{title}': {} entries", entries.len());
        assert!(
            entries.len() >= 200,
            "test needs a 200+ track playlist, got {}",
            entries.len()
        );

        // ---- Initial metadata load ----------------------------------------
        let started = Instant::now();
        pipeline::load_playlist(settings.clone(), playlist.clone(), tx.clone(), token.clone())
            .await;
        let load = collect(&rx);
        println!("initial load took {:?}", started.elapsed());
        let (load_cancelled, load_failed, load_rate_limited) =
            load.load_finished.expect("load must announce completion");
        assert!(!load_cancelled);
        println!("load reported: {load_failed} failed, {load_rate_limited} rate limited");

        let ok_before: HashMap<usize, TrackMetadata> = load
            .meta
            .iter()
            .filter_map(|(i, r)| r.as_ref().ok().map(|m| (*i, m.clone())))
            .collect();
        // The GUI retries only what could still succeed; tracks that are gone
        // for good are reported, not re-queued. Mirror that split here.
        let failed = load.retryable_failures();
        let dead = load.permanent_failures();
        println!(
            "initial load: {} ok, {} retryable failures, {} permanently unavailable (of {})",
            ok_before.len(),
            failed.len(),
            dead.len(),
            entries.len()
        );
        assert_eq!(
            dead.len(),
            load.load_permanent,
            "permanent count must match the rows actually marked permanent"
        );
        for i in &dead {
            let Err(e) = &load.meta[i] else { unreachable!() };
            println!("  row {i} unavailable: {}", e.headline());
            assert!(e.reason.is_some(), "a permanent failure must carry a human reason");
        }

        let restricted = load.restricted_failures();
        assert_eq!(restricted.len(), load.load_restricted);
        for i in &restricted {
            let Err(e) = &load.meta[i] else { unreachable!() };
            println!("  row {i} restricted: {}", e.headline());
            // Restricted is explicitly not a claim that the track is gone.
            assert!(!e.is_permanent(), "restricted must not be reported as unavailable");
        }
        if failed.is_empty() {
            println!("nothing retryable failed on this run — retry path not exercised");
            return;
        }

        // ---- Retry only the failures --------------------------------------
        let targets: Vec<(usize, FlatEntry)> =
            failed.iter().map(|i| (*i, entries[*i].clone())).collect();
        let started = Instant::now();
        pipeline::retry_failed_metadata(settings, String::new(), targets, tx, token).await;
        let retry = collect(&rx);
        println!("retry took {:?}", started.elapsed());

        let (recovered, still_failed, cancelled, gave_up) =
            retry.finished.expect("retry must finish");
        println!(
            "retry: {recovered} recovered, {still_failed} still failed\
             , gave_up={gave_up}"
        );
        assert!(!cancelled);
        assert_eq!(recovered + still_failed, failed.len());

        // Successfully loaded tracks were never re-fetched.
        for i in ok_before.keys() {
            assert!(
                !retry.meta.contains_key(i),
                "retry touched already-loaded row {i}"
            );
            assert!(!retry.retrying.contains(i));
        }
        // Every retried row is one that had failed.
        for i in retry.meta.keys() {
            assert!(failed.contains(i), "retry touched row {i} that had not failed");
        }
        // Regression: permanently dead tracks must never be re-queued.
        for i in &dead {
            assert!(
                !retry.retrying.contains(i),
                "row {i} is permanently unavailable but was retried anyway"
            );
        }
        assert!(recovered > 0, "expected at least some tracks to recover");
    });
}
