//! Orchestrates the background work: playlist loading (flat list + per-track
//! metadata enrichment), download -> validate -> render, progress reporting
//! and cancellation.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::settings::{CookieBrowser, Settings};
use crate::downloader::cookies::{self, CookieSource};
use crate::downloader::metadata::DownloadedTrack;
use crate::downloader::ytdlp::RetryDecision;
use crate::downloader::{metadata, ytdlp};
use crate::models::cache::{CachedMeta, CachedRender, PlaylistCache};
use crate::models::messages::{Tx, WorkerMsg};
use crate::models::track::{MetaFailure, Track, TrackStatus};
use crate::utils::filesystem::{ensure_dir, sanitize_filename, unique_path};
use crate::utils::process::version_of;
use crate::video::checkpoint::Checkpoint;
use crate::video::concat::{ClipInfo, PlaylistRenderer};
use crate::video::encoder::{self, ResolvedEncoder};
use crate::video::renderer;

/// How many parallel `--dump-single-json` metadata fetches to run.
const META_CONCURRENCY: usize = 4;

/// Probe for ffmpeg / yt-dlp and report versions to the GUI.
pub async fn check_tools(ffmpeg: String, ytdlp: String, tx: Tx) {
    let (ff, yt) = tokio::join!(
        version_of(&ffmpeg, "-version"),
        version_of(&ytdlp, "--version"),
    );
    tx.send(WorkerMsg::Tools {
        ffmpeg: ff,
        ytdlp: yt.map(|v| format!("yt-dlp {v}")),
    });
}

/// Probe which H.264 encoders this machine can actually use (functional test —
/// see [`crate::video::encoder`]) and report them to the GUI.
pub async fn detect_encoders(ffmpeg: String, tx: Tx) {
    let support = encoder::detect(&ffmpeg).await;
    if !support.gpus.is_empty() {
        tx.log(format!("Detected GPU(s): {}", support.gpus.join(", ")));
    }
    for (name, avail) in [
        ("NVENC", &support.nvenc),
        ("Quick Sync", &support.qsv),
        ("AMF", &support.amf),
    ] {
        match avail.reason() {
            None => tx.log(format!("Encoder {name}: available")),
            Some(reason) => tx.log(format!("Encoder {name}: unavailable ({reason})")),
        }
    }
    tx.send(WorkerMsg::Encoders(support));
}

/// Render a 30-second synthetic sample (a static background with a text overlay,
/// the same shape as the real workload) with `encoder`, timing it so CPU and GPU
/// can be compared before a multi-hour render. Reports elapsed time, average FPS
/// and output size.
pub async fn benchmark_encoder(
    settings: Settings,
    encoder: ResolvedEncoder,
    tx: Tx,
    token: CancellationToken,
) {
    tx.log(format!(
        "Benchmarking {} with a 30s sample...",
        encoder.kind.full_label()
    ));
    let result = run_benchmark(&settings, encoder, &token, &tx)
        .await
        .map_err(|e| format!("{e:#}"));
    match &result {
        Ok(r) => tx.log(format!(
            "Benchmark: {} — {:.1}s, {:.0} fps, {:.1} MB",
            r.encoder,
            r.elapsed_s,
            r.fps,
            r.size_bytes as f64 / 1_048_576.0
        )),
        Err(e) => tx.log(format!("Benchmark failed: {e}")),
    }
    tx.send(WorkerMsg::BenchmarkDone(result));
}

const BENCHMARK_SECONDS: u32 = 30;
const BENCHMARK_FPS: u32 = 30;

async fn run_benchmark(
    settings: &Settings,
    encoder: ResolvedEncoder,
    token: &CancellationToken,
    tx: &Tx,
) -> anyhow::Result<crate::models::messages::BenchmarkResult> {
    use crate::utils::process::{run_streaming, tool_command};

    let (w, h) = settings.resolution();
    let out = std::env::temp_dir().join("soundcloud2mp4_benchmark.mp4");
    let _ = std::fs::remove_file(&out);

    // A detailed animated test pattern gives the encoder realistic work, so the
    // CPU-vs-GPU throughput comparison is meaningful. Deliberately no `drawtext`:
    // without a `fontfile` it falls back to fontconfig, which is unconfigured on
    // Windows and crashes ffmpeg — and text is a negligible, encoder-independent
    // cost anyway. (A full-motion pattern is harder to compress than the app's
    // mostly-static covers, so the resulting estimate is conservative.)
    let mut cmd = tool_command(&settings.ffmpeg_path);
    cmd.args([
        "-y".to_string(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("testsrc2=s={w}x{h}:r={BENCHMARK_FPS}:d={BENCHMARK_SECONDS}"),
    ]);
    cmd.args(encoder.video_args(crate::video::encoder::QualityTier::Final));
    cmd.args(["-an".to_string(), out.to_string_lossy().into_owned()]);

    let started = Instant::now();
    run_streaming(cmd, &settings.ffmpeg_path, token, tx, "benchmark", None).await?;
    let elapsed_s = started.elapsed().as_secs_f64().max(0.001);

    let size_bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let frames = (BENCHMARK_SECONDS * BENCHMARK_FPS) as f64;
    let _ = std::fs::remove_file(&out);

    Ok(crate::models::messages::BenchmarkResult {
        encoder: encoder.kind.full_label(),
        elapsed_s,
        fps: frames / elapsed_s,
        size_bytes,
    })
}

/// Step 1: flat playlist fetch -> placeholder rows appear immediately.
/// Step 2: per-track `--dump-single-json` fetches fill in title / uploader /
/// duration / thumbnail as they arrive (TrackMeta messages).
pub async fn load_playlist(settings: Settings, url: String, tx: Tx, token: CancellationToken) {
    if !ytdlp::looks_like_soundcloud_url(&url) {
        tx.send(WorkerMsg::Playlist(Err(
            "That does not look like a SoundCloud URL. Expected something like \
             https://soundcloud.com/artist/sets/playlist-name"
                .into(),
        )));
        return;
    }

    let ytdlp_path = settings.ytdlp_path.clone();
    let log_file = settings.tool_log("yt-dlp.log");
    let cookies = settings.cookie_source();
    // Log the cookie *mode* on every load. Never the cookie values — the app
    // only ever hands yt-dlp a browser name or a path, and never reads a jar.
    tx.log(format!("SoundCloud authentication: {}", cookies.describe()));
    if settings.cookies_missing() {
        tx.log(format!(
            "WARN: cookies file '{}' does not exist — ignoring it.",
            settings.cookies_path.trim()
        ));
    } else if settings.cookies_wrong_format() {
        tx.log(format!(
            "WARN: cookies file '{}' is not in Netscape format (most browser export \
             extensions save JSON, which yt-dlp cannot read) — ignoring it.",
            settings.cookies_path.trim()
        ));
    }

    tx.log(format!("Loading playlist: {url}"));
    let (title, entries) = match ytdlp::fetch_playlist(
        &ytdlp_path,
        &url,
        &cookies,
        &token,
        &tx,
        log_file.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        // Offline / rate-limited on the *flat* fetch: if this playlist is
        // cached from a previous run, fall back to the cached membership so the
        // list is still usable (and recovery can fill in the rest later),
        // instead of failing outright.
        Err(e) => match PlaylistCache::load(&url) {
            Some(cache) if !cache.tracks.is_empty() => {
                tx.log(format!(
                    "Could not reach SoundCloud ({e:#}); using the cached copy of this \
                     playlist ({} track(s)).",
                    cache.tracks.len()
                ));
                let entries = cache
                    .tracks
                    .iter()
                    .map(|t| ytdlp::FlatEntry {
                        id: t.id.clone(),
                        url: t.url.clone(),
                        title: Some(t.title.clone()),
                    })
                    .collect::<Vec<_>>();
                (cache.playlist_title.clone(), entries)
            }
            _ => {
                tx.send(WorkerMsg::Playlist(Err(format!("{e:#}"))));
                return;
            }
        },
    };

    // ---- Load / reconcile the persistent metadata cache -------------------
    // Keyed per playlist, so switching between playlists never mixes them and a
    // reload of this one costs no SoundCloud requests for metadata already held.
    let mut cache_state = PlaylistCache::load_or_new(&url, &title);
    cache_state.sync_entries(&title, &entries);
    let cached_complete = cache_state.complete_count();
    cache_state.save();

    // Build the initial list, hydrating every track we already know from cache.
    let global_max = settings.max_track_seconds;
    let tracks: Vec<Track> = cache_state
        .tracks
        .iter()
        .map(|c| c.to_track(global_max))
        .collect();
    let total = tracks.len();
    if cached_complete > 0 {
        tx.log(format!(
            "✓ Loaded metadata cache ({cached_complete} of {total} track(s) already known)"
        ));
    }
    tx.send(WorkerMsg::Playlist(Ok((title, tracks))));

    // Only tracks that are not already complete — and not known to be
    // permanently gone (those never resolve) — need a SoundCloud request.
    let to_fetch: Vec<(usize, ytdlp::FlatEntry)> = cache_state
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            !c.is_complete()
                && !matches!(c.meta, CachedMeta::Failed { permanent: true, .. })
        })
        .map(|(i, _)| (i, entries[i].clone()))
        .collect();

    if to_fetch.is_empty() {
        tx.log("All track metadata served from cache — no SoundCloud requests needed.");
        tx.send(WorkerMsg::MetaLoadFinished {
            cancelled: token.is_cancelled(),
            failed: 0,
            rate_limited: 0,
            permanent: 0,
            restricted: 0,
        });
        return;
    }
    // The initial load is now a *resilient* fetch: a transient failure (HTTP
    // 429, 5xx, network trouble) pauses the whole batch on a shared cooldown and
    // retries with exponential back-off, instead of skipping the track. A brief
    // SoundCloud rate limit therefore no longer strands dozens of tracks.
    let max_attempts = if settings.auto_metadata_retry {
        settings.metadata_retry_attempts()
    } else {
        1
    };
    let fetch = Arc::new(MetaFetch::new(&settings, Duration::ZERO, max_attempts));
    tx.log(format!(
        "Fetching metadata for {} track(s) not in cache ({} at a time, up to {} attempt(s) each{}).",
        to_fetch.len(),
        META_CONCURRENCY,
        max_attempts,
        if settings.auto_metadata_retry {
            format!(", rate-limit back-off {}", describe_backoff(&fetch.gate.steps))
        } else {
            String::new()
        }
    ));

    // ---- Step 2: metadata enrichment (only the missing tracks) ------------
    let cache = Arc::new(std::sync::Mutex::new(cache_state));
    let semaphore = Arc::new(Semaphore::new(META_CONCURRENCY));
    let failed = Arc::new(AtomicUsize::new(0));
    // Failures that look like rate limiting, i.e. worth an automatic retry.
    let rate_limited = Arc::new(AtomicUsize::new(0));
    // Tracks that are gone for good — never retried, reported as unavailable.
    let permanent = Arc::new(AtomicUsize::new(0));
    // Tracks SoundCloud withheld from this client; cookies may help.
    let restricted = Arc::new(AtomicUsize::new(0));
    let mut join_set = tokio::task::JoinSet::new();
    for (index, entry) in to_fetch {
        let permit_source = semaphore.clone();
        let tx = tx.clone();
        let token = token.clone();
        let failed = failed.clone();
        let rate_limited = rate_limited.clone();
        let permanent = permanent.clone();
        let restricted = restricted.clone();
        let cache = cache.clone();
        let fetch = fetch.clone();
        join_set.spawn(async move {
            let _permit = permit_source.acquire().await;
            if token.is_cancelled() {
                return;
            }
            let result = fetch_track_resilient(&fetch, &entry, &token, &tx).await;
            if token.is_cancelled() {
                return;
            }
            if let Err(f) = &result {
                failed.fetch_add(1, Ordering::Relaxed);
                match f.kind {
                    ytdlp::FailureKind::Retryable => rate_limited.fetch_add(1, Ordering::Relaxed),
                    ytdlp::FailureKind::Permanent => permanent.fetch_add(1, Ordering::Relaxed),
                    ytdlp::FailureKind::Restricted => restricted.fetch_add(1, Ordering::Relaxed),
                };
            }
            // Persist the outcome immediately so a crash or restart keeps it.
            update_cache_meta(&cache, index, &result);
            tx.send(WorkerMsg::TrackMeta(index, result));
        });
    }
    while join_set.join_next().await.is_some() {}

    let failed = failed.load(Ordering::Relaxed);
    let rate_limited = rate_limited.load(Ordering::Relaxed);
    let permanent = permanent.load(Ordering::Relaxed);
    let restricted = restricted.load(Ordering::Relaxed);
    if !token.is_cancelled() {
        tx.log(format!(
            "Playlist metadata loading finished: {failed} still failed \
             ({rate_limited} transient/rate-limited, {restricted} access-restricted, \
             {permanent} permanently unavailable)."
        ));
    }
    tx.send(WorkerMsg::MetaLoadFinished {
        cancelled: token.is_cancelled(),
        failed,
        rate_limited,
        permanent,
        restricted,
    });
}

/// Human-readable back-off ladder, e.g. `5s → 10s → 20s → 40s → 60s`.
fn describe_backoff(steps: &[Duration]) -> String {
    steps
        .iter()
        .map(|d| format!("{}s", d.as_secs()))
        .collect::<Vec<_>>()
        .join(" → ")
}

/// Persist one metadata outcome to the shared cache (crash-safe). Shared by the
/// initial load and the recovery pass so a resolved track is never re-fetched
/// after a restart.
fn update_cache_meta(
    cache: &Arc<std::sync::Mutex<PlaylistCache>>,
    index: usize,
    result: &Result<crate::models::track::TrackMetadata, MetaFailure>,
) {
    let mut c = cache.lock().unwrap();
    match result {
        Ok(meta) => c.record_meta(index, meta),
        Err(f) => c.record_meta_failure(index, f),
    }
    c.save();
}

/// Give up on the whole pass after this many rate-limit hits with no success
/// in between — at that point SoundCloud wants a longer break than a retry
/// pass should hold the GUI for, and the manual button is the right next step.
///
/// Any success resets the counter, so this only trips on a *sustained* block.
/// Sized against a typical retry success rate (on the order of 25%): a run of
/// 15 failures is ~1% likely by chance, while 8 would have been ~10% and aborted
/// healthy passes early.
const ABORT_AFTER_CONSECUTIVE_PENALTIES: usize = 15;

/// A back-off shared by every retry worker.
///
/// Rate limiting is a property of the *connection*, not of one track, so a
/// per-track sleep would be both too slow (each worker serving its own penalty)
/// and too aggressive (other workers hammering on regardless). One gate means a
/// single 30 s pause slows the entire pass and then releases all workers at
/// once.
struct BackoffGate {
    /// Rungs of the ladder, e.g. 5s → 15s → 30s. Never empty.
    steps: Vec<Duration>,
    state: std::sync::Mutex<GateState>,
}

struct GateState {
    /// No request may start before this instant.
    open_at: Instant,
    /// Next rung of `steps` to apply.
    level: usize,
    /// Penalties since the last success; trips the abort guard.
    consecutive: usize,
}

impl BackoffGate {
    fn new(steps: Vec<Duration>) -> Self {
        Self {
            steps: if steps.is_empty() {
                vec![Duration::from_secs(5)]
            } else {
                steps
            },
            state: std::sync::Mutex::new(GateState {
                open_at: Instant::now(),
                level: 0,
                consecutive: 0,
            }),
        }
    }

    /// Wait for the gate to open, then observe the per-request spacing.
    /// Returns false if cancelled while waiting.
    async fn wait(&self, spacing: Duration, token: &CancellationToken) -> bool {
        loop {
            let remaining = {
                let s = self.state.lock().unwrap();
                s.open_at.saturating_duration_since(Instant::now())
            };
            if remaining.is_zero() {
                break;
            }
            // Re-check afterwards: another worker may have pushed the gate out
            // further while this one slept.
            if !sleep_or_cancel(remaining, token).await {
                return false;
            }
        }
        sleep_or_cancel(spacing, token).await
    }

    /// Record a rate-limit/transient failure and close the gate for the next
    /// rung of the ladder. Returns the applied back-off and whether the pass
    /// should give up entirely.
    fn penalize(&self) -> (Duration, bool) {
        let mut s = self.state.lock().unwrap();
        let last = self.steps.len() - 1;
        let step = self.steps[s.level.min(last)];
        s.level = (s.level + 1).min(last);
        s.consecutive += 1;
        let open_at = Instant::now() + step;
        // Never pull the gate back in: a longer penalty already set by another
        // worker wins.
        if open_at > s.open_at {
            s.open_at = open_at;
        }
        (step, s.consecutive >= ABORT_AFTER_CONSECUTIVE_PENALTIES)
    }

    /// A success means the rate limit is easing — step back down the ladder.
    fn relax(&self) {
        let mut s = self.state.lock().unwrap();
        s.level = s.level.saturating_sub(1);
        s.consecutive = 0;
    }
}

/// Shared configuration + global state for one resilient metadata-fetch batch.
///
/// Both the initial load and the on-demand retry pass build a `MetaFetch` and
/// hand every worker a clone of the same `Arc`, so there is a **single** retry
/// implementation ([`fetch_track_resilient`]) and a **single** cooldown across
/// all of a batch's workers. The two callers differ only in pacing (the load
/// runs at full concurrency with no per-request delay; the retry pass is
/// deliberately gentler).
struct MetaFetch {
    ytdlp_path: String,
    /// The user's configured cookie source for the first attempt.
    cookies: CookieSource,
    /// The selected browser, if any — the target of the one-shot cookie refresh.
    browser: Option<CookieBrowser>,
    auto_cookie_refresh: bool,
    log_file: Option<std::path::PathBuf>,
    /// Per-request spacing (zero for the initial load).
    delay: Duration,
    /// Total normal attempts per track (>= 1). One gate rung per attempt.
    max_attempts: u32,
    /// Shared global cooldown — one rate limit pauses every worker in the batch.
    gate: Arc<BackoffGate>,
    /// Set when the gate tops out; workers stop instead of grinding forever.
    gave_up: Arc<AtomicBool>,
}

impl MetaFetch {
    fn new(settings: &Settings, delay: Duration, max_attempts: u32) -> Self {
        let browser = (settings.cookie_browser != CookieBrowser::None)
            .then_some(settings.cookie_browser);
        Self {
            ytdlp_path: settings.ytdlp_path.clone(),
            cookies: settings.cookie_source(),
            browser,
            auto_cookie_refresh: settings.auto_cookie_refresh,
            log_file: settings.tool_log("yt-dlp.log"),
            delay,
            max_attempts: max_attempts.max(1),
            gate: Arc::new(BackoffGate::new(settings.retry_backoff())),
            gave_up: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Whether an access-restricted failure should trigger the one-shot cookie
/// refresh: only when the feature is on, it has not already been done for this
/// track, and a browser is actually configured to refresh from. Pure so the
/// gating can be unit-tested without a browser or yt-dlp.
fn should_refresh_cookies(
    auto_cookie_refresh: bool,
    already_refreshed: bool,
    browser: Option<CookieBrowser>,
) -> bool {
    auto_cookie_refresh && !already_refreshed && browser.is_some()
}

/// Fetch one track's metadata resiliently. The retry brain shared by the initial
/// load and the retry pass:
///
/// - **Transient** failure (HTTP 429 / 5xx / timeout / connection reset / DNS /
///   TLS / proxy, per [`RetryDecision::Retry`]): close the *shared* gate so every
///   worker pauses, then retry with exponential back-off up to `max_attempts`.
/// - **Access-restricted** failure (DRM / auth / login / expired cookies, per
///   [`RetryDecision::RefreshCookiesAndRetry`]): if a browser is configured and
///   cookie refresh is on, re-extract that browser's cookies **once** and retry
///   the request a single time, then classify normally.
/// - **Permanent** failure (deleted / private / 404 / unsupported / malformed,
///   per [`RetryDecision::PermanentFailure`]): stop immediately.
///
/// Returns resolved metadata, or the final [`MetaFailure`] carrying the verdict
/// for the GUI row and the cache. Every decision is logged.
async fn fetch_track_resilient(
    m: &MetaFetch,
    entry: &ytdlp::FlatEntry,
    token: &CancellationToken,
    tx: &Tx,
) -> Result<crate::models::track::TrackMetadata, MetaFailure> {
    let mut last_error: Option<MetaFailure> = None;
    // Which cookie source the *next* attempt uses; switches to a freshly-probed
    // browser jar after the single cookie refresh.
    let mut cookies = m.cookies.clone();
    let mut cookies_refreshed = false;
    let cancelled_msg = || MetaFailure::transient("metadata fetch cancelled");

    let mut attempt: u32 = 0;
    while attempt < m.max_attempts {
        attempt += 1;

        // Wait out any global cooldown another worker triggered, then observe
        // the per-request spacing.
        if !m.gate.wait(m.delay, token).await {
            return Err(last_error.unwrap_or_else(cancelled_msg));
        }
        if m.gave_up.load(Ordering::Relaxed) {
            return Err(last_error.unwrap_or_else(|| {
                MetaFailure::transient(
                    "SoundCloud is still rate limiting — try again in a few minutes",
                )
            }));
        }

        let outcome = ytdlp::fetch_track_info_detailed(
            &m.ytdlp_path,
            entry,
            &cookies,
            token,
            tx,
            m.log_file.as_deref(),
        )
        .await;

        let e = match outcome {
            Ok(meta) => {
                m.gate.relax();
                tx.log(format!(
                    "✓ Metadata downloaded — track {} \"{}\" (attempt {attempt}/{})",
                    entry.id, meta.title, m.max_attempts
                ));
                return Ok(meta);
            }
            Err(e) => e,
        };

        if e.cancelled || token.is_cancelled() {
            return Err(last_error.unwrap_or_else(|| MetaFailure::transient(e.message.clone())));
        }
        let first_line = e.message.lines().next().unwrap_or("unknown error").to_string();

        match e.decision() {
            RetryDecision::PermanentFailure => {
                let reason = e.reason.unwrap_or(ytdlp::NO_DATA_REASON);
                tx.log(format!(
                    "Track {} is permanently unavailable ({reason}) — not retrying ({first_line})",
                    entry.id
                ));
                return Err(MetaFailure::permanent(e.message, reason));
            }
            RetryDecision::RefreshCookiesAndRetry => {
                let reason = e.reason.unwrap_or(ytdlp::RESTRICTED_REASON);
                // One cookie refresh + one extra retry, then classify normally.
                if should_refresh_cookies(m.auto_cookie_refresh, cookies_refreshed, m.browser) {
                    if let Some(browser) = m.browser {
                        cookies_refreshed = true;
                        tx.log(format!(
                            "Track {}: {reason}. Refreshing browser cookies ({}) and retrying once...",
                            entry.id,
                            browser.label()
                        ));
                        // Re-extract the browser jar via the existing cookie code
                        // (offline probe), so a session refreshed since the last
                        // request is picked up. Then retry with those cookies.
                        let status =
                            cookies::probe(&m.ytdlp_path, browser, token, m.log_file.as_deref())
                                .await;
                        tx.log(format!("Cookie refresh: {}. Retrying...", status.headline()));
                        cookies = CookieSource::Browser(browser);
                        last_error = Some(MetaFailure::restricted(e.message.clone(), reason));
                        // Redo this attempt with the refreshed cookies rather than
                        // spending it, so the refresh gets its own try even when
                        // the restriction surfaced on the final attempt.
                        attempt -= 1;
                        continue;
                    }
                }
                if cookies_refreshed {
                    tx.log(format!(
                        "Track {}: {reason} — still refused after refreshing cookies ({first_line})",
                        entry.id
                    ));
                } else {
                    tx.log(format!(
                        "Track {}: {reason}. Not retried automatically — select a browser under \
                         Settings → SoundCloud Authentication to try signed in ({first_line})",
                        entry.id
                    ));
                }
                return Err(MetaFailure::restricted(e.message, reason));
            }
            RetryDecision::Retry => {
                last_error = Some(MetaFailure::transient(e.message.clone()));
                // Close the SHARED gate so every worker slows, not just this one.
                let (backoff, abort) = m.gate.penalize();
                if abort {
                    m.gave_up.store(true, Ordering::Relaxed);
                }
                let last_attempt = attempt >= m.max_attempts;
                tx.log(format!(
                    "⏸ Rate limited / transient error on track {} (attempt {attempt}/{}) — \
                     global cooldown {}s{} ({first_line})",
                    entry.id,
                    m.max_attempts,
                    backoff.as_secs(),
                    if abort {
                        "; giving up on this batch (background recovery will continue)"
                    } else if last_attempt {
                        "; maximum retries exceeded — track marked as failed"
                    } else {
                        ", retrying"
                    }
                ));
                if abort || last_attempt {
                    return Err(last_error.unwrap_or_else(|| MetaFailure::transient(e.message)));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(cancelled_msg))
}

/// Re-fetch metadata for tracks whose first attempt failed, using the same
/// resilient [`fetch_track_resilient`] engine as the initial load but paced far
/// more gently: few workers, a delay before every request, and the shared
/// exponential back-off ladder whenever SoundCloud signals rate limiting.
///
/// Runs automatically after a load that hit rate limiting, and on demand from
/// the "Retry Failed Metadata" button.
///
/// `targets` carries the GUI row index for each entry, so results land on the
/// right row — successfully loaded tracks are never touched.
pub async fn retry_failed_metadata(
    settings: Settings,
    playlist_url: String,
    targets: Vec<(usize, ytdlp::FlatEntry)>,
    tx: Tx,
    token: CancellationToken,
) {
    let total = targets.len();
    if total == 0 {
        tx.send(WorkerMsg::MetaRetryFinished {
            recovered: 0,
            still_failed: 0,
            permanent: 0,
            restricted: 0,
            cancelled: false,
            gave_up: false,
        });
        return;
    }

    let workers = settings.retry_workers().min(META_CONCURRENCY);
    let attempts = settings.metadata_retry_attempts();
    let fetch = Arc::new(MetaFetch::new(&settings, settings.retry_delay(), attempts));
    tx.log(format!(
        "Retrying metadata for {total} failed track(s): {workers} worker(s), \
         {}ms between requests, up to {attempts} attempts each, \
         rate-limit back-off {}.",
        settings.retry_delay().as_millis(),
        describe_backoff(&fetch.gate.steps)
    ));
    tx.send(WorkerMsg::MetaRetryProgress { remaining: total, total });

    let semaphore = Arc::new(Semaphore::new(workers));
    let done = Arc::new(AtomicUsize::new(0));
    let recovered = Arc::new(AtomicUsize::new(0));
    // Tracks this pass proved are gone for good; the GUI stops offering them.
    let permanent = Arc::new(AtomicUsize::new(0));
    // Tracks SoundCloud withheld from this client.
    let restricted = Arc::new(AtomicUsize::new(0));
    // Persist every recovered/failed track to the playlist cache as it resolves,
    // so recovery survives a restart and never re-fetches what already loaded.
    let cache = (!playlist_url.is_empty())
        .then(|| PlaylistCache::load(&playlist_url).map(|c| Arc::new(std::sync::Mutex::new(c))))
        .flatten();

    let mut join_set = tokio::task::JoinSet::new();
    for (index, entry) in targets {
        let permit_source = semaphore.clone();
        let tx = tx.clone();
        let token = token.clone();
        let done = done.clone();
        let recovered = recovered.clone();
        let permanent = permanent.clone();
        let restricted = restricted.clone();
        let fetch = fetch.clone();
        let cache = cache.clone();

        join_set.spawn(async move {
            let _permit = permit_source.acquire().await;
            if token.is_cancelled() || fetch.gave_up.load(Ordering::Relaxed) {
                return;
            }
            tx.send(WorkerMsg::MetaRetrying(index));

            let result = fetch_track_resilient(&fetch, &entry, &token, &tx).await;

            if token.is_cancelled() {
                return;
            }
            match &result {
                Ok(_) => {
                    recovered.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) if e.is_permanent() => {
                    permanent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) if e.is_restricted() => {
                    restricted.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {}
            }
            if let Some(cache) = &cache {
                update_cache_meta(cache, index, &result);
            }
            tx.send(WorkerMsg::TrackMeta(index, result));

            let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
            tx.send(WorkerMsg::MetaRetryProgress {
                remaining: total.saturating_sub(finished),
                total,
            });
        });
    }
    while join_set.join_next().await.is_some() {}

    let recovered = recovered.load(Ordering::Relaxed);
    let permanent = permanent.load(Ordering::Relaxed);
    let restricted = restricted.load(Ordering::Relaxed);
    let cancelled = token.is_cancelled();
    let gave_up = fetch.gave_up.load(Ordering::Relaxed);
    // Anything not recovered is still failed — including tracks skipped by a
    // cancellation or by the abort guard, which keep the error they had.
    let still_failed = total - recovered;
    tx.log(format!(
        "Metadata retry {}: {recovered} recovered, {still_failed} still failed \
         ({restricted} access-restricted, {permanent} permanently unavailable) (of {total}).",
        if cancelled {
            "cancelled"
        } else if gave_up {
            "stopped early (still rate limited)"
        } else {
            "complete"
        }
    ));
    tx.send(WorkerMsg::MetaRetryFinished {
        recovered,
        still_failed,
        permanent,
        restricted,
        cancelled,
        gave_up,
    });
}

/// Sleep unless cancelled first. Returns false if cancellation won, so callers
/// stop immediately instead of finishing the delay.
async fn sleep_or_cancel(d: std::time::Duration, token: &CancellationToken) -> bool {
    tokio::select! {
        _ = token.cancelled() => false,
        _ = tokio::time::sleep(d) => true,
    }
}

/// Sequentially convert the given (index, track) pairs.
pub async fn convert(
    settings: Settings,
    tracks: Vec<(usize, Track)>,
    encoder: ResolvedEncoder,
    tx: Tx,
    token: CancellationToken,
) {
    let total = tracks.len();
    let mut ok = 0usize;
    let mut failed = 0usize;

    tx.send(WorkerMsg::EncoderActive(encoder.kind.full_label()));

    if let Err(e) = ensure_dir(&settings.output_folder) {
        tx.log(format!("ERROR: cannot create output folder: {e:#}"));
        tx.send(WorkerMsg::Finished { ok, failed: total, cancelled: false });
        return;
    }
    let work_root = settings.output_folder.join(".work");
    let ytdlp_log = settings.tool_log("yt-dlp.log");

    for (n, (index, track)) in tracks.into_iter().enumerate() {
        if token.is_cancelled() {
            tx.send(WorkerMsg::Finished { ok, failed, cancelled: true });
            return;
        }

        let label = format!("{} - {}", track.title, track.uploader);
        let workdir = work_root.join(format!("track_{index:04}"));
        let _ = std::fs::remove_dir_all(&workdir);
        if let Err(e) = ensure_dir(&workdir) {
            tx.log(format!("ERROR: {e:#}"));
            failed += 1;
            tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Failed(format!("{e:#}"))));
            continue;
        }

        // ---- Download ------------------------------------------------------
        tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Downloading));
        tx.send(WorkerMsg::Progress {
            phase: format!("Downloading Track {}/{total}: {label}", n + 1),
            frac: n as f32 / total as f32,
        });

        let result = async {
            ytdlp::download_track(
                &settings.ytdlp_path,
                &track.url,
                &workdir,
                settings.bitrate_k(),
                &settings.cookie_source(),
                &token,
                &tx,
                ytdlp_log.as_deref(),
            )
            .await?;

            tx.send(WorkerMsg::Progress {
                phase: format!("Processing metadata... ({label})"),
                frac: (n as f32 + 0.45) / total as f32,
            });
            // Validates audio / info.json / thumbnail presence and resolves
            // final metadata with fallbacks; fails with "Download incomplete:
            // missing ..." naming exactly what is absent.
            let downloaded = metadata::read_downloaded(&workdir)?;
            tx.log(format!(
                "Validated download: audio={}, cover={}, title={}, uploader={}, duration={}s",
                downloaded.audio.file_name().unwrap_or_default().to_string_lossy(),
                downloaded
                    .cover
                    .as_ref()
                    .map(|c| c.file_name().unwrap_or_default().to_string_lossy().into_owned())
                    .unwrap_or_else(|| "(none)".into()),
                downloaded.meta.title,
                downloaded.meta.uploader,
                downloaded.meta.duration,
            ));

            tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Rendering));
            tx.send(WorkerMsg::Progress {
                phase: format!("Rendering video... ({label})"),
                frac: (n as f32 + 0.55) / total as f32,
            });
            renderer::render_track(
                &settings,
                &downloaded,
                track.play_seconds,
                encoder,
                &workdir,
                &token,
                &tx,
            )
            .await
        }
        .await;

        match result {
            Ok(path) => {
                ok += 1;
                tx.log(format!("Finished: {}", path.display()));
                tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Done(path)));
                let _ = std::fs::remove_dir_all(&workdir);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if token.is_cancelled() || msg.contains("cancelled") {
                    tx.log(format!("Cancelled during: {label}"));
                    tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Pending));
                    tx.send(WorkerMsg::Finished { ok, failed, cancelled: true });
                    let _ = std::fs::remove_dir_all(&workdir);
                    return;
                }
                failed += 1;
                // Withheld-access failures get the cookies hint appended, so the
                // row explains what might actually help.
                let msg = ytdlp::annotate_download_failure(&msg);
                tx.log(format!("ERROR on '{label}': {msg}"));
                tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Failed(msg)));
            }
        }
    }

    let _ = std::fs::remove_dir_all(&work_root);
    tx.send(WorkerMsg::Progress { phase: "Done".into(), frac: 1.0 });
    tx.send(WorkerMsg::Finished { ok, failed, cancelled: false });
}

/// Measured duration of an already-rendered clip when it exists and ffprobe can
/// read it — the signal that a combined-export run may reuse it instead of
/// downloading and rendering the track again.
///
/// Returns `None` when the file is absent or ffprobe cannot parse a duration
/// (e.g. a clip a crash left half-written), so the caller regenerates it. This
/// is the ffprobe-based validity check the resume feature relies on, rather than
/// trusting that a file on disk is complete just because it is present.
async fn reusable_clip(
    ffprobe: &str,
    clip_path: &std::path::Path,
    token: &CancellationToken,
    log_file: Option<&std::path::Path>,
) -> Option<f64> {
    if !clip_path.exists() {
        return None;
    }
    crate::video::probe::probe_duration(ffprobe, clip_path, token, log_file)
        .await
        .ok()
}

/// Download a track and validate its files. Shared by both export modes.
async fn download_and_validate(
    settings: &Settings,
    url: &str,
    workdir: &std::path::Path,
    token: &CancellationToken,
    tx: &Tx,
) -> anyhow::Result<DownloadedTrack> {
    ytdlp::download_track(
        &settings.ytdlp_path,
        url,
        workdir,
        settings.bitrate_k(),
        &settings.cookie_source(),
        token,
        tx,
        settings.tool_log("yt-dlp.log").as_deref(),
    )
    .await?;
    let downloaded = metadata::read_downloaded(workdir)?;
    tx.log(format!(
        "Validated download: audio={}, cover={}, title={}, uploader={}, duration={}s",
        downloaded.audio.file_name().unwrap_or_default().to_string_lossy(),
        downloaded
            .cover
            .as_ref()
            .map(|c| c.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .unwrap_or_else(|| "(none)".into()),
        downloaded.meta.title,
        downloaded.meta.uploader,
        downloaded.meta.duration,
    ));
    Ok(downloaded)
}

/// Combined-playlist export: download + render every selected track to a
/// fixed-length intermediate clip, then stitch them into one MP4 with
/// transitions, playlist metadata and chapters.
pub async fn convert_combined(
    settings: Settings,
    playlist_name: String,
    playlist_url: String,
    tracks: Vec<(usize, Track)>,
    encoder: ResolvedEncoder,
    tx: Tx,
    token: CancellationToken,
) {
    let total = tracks.len();
    tx.send(WorkerMsg::EncoderActive(encoder.kind.full_label()));

    // The persistent metadata cache also carries per-track render status, so a
    // resumed run can show what was already produced. Advisory only — the
    // ffprobe-validated clip/batch reuse remains the authority.
    let cache = (!playlist_url.is_empty())
        .then(|| PlaylistCache::load(&playlist_url).map(|c| Arc::new(std::sync::Mutex::new(c))))
        .flatten();
    let record_render = |index: usize, render: CachedRender, clip: Option<String>| {
        if let Some(cache) = &cache {
            let mut c = cache.lock().unwrap();
            c.record_render(index, render, clip);
            c.save();
        }
    };

    if let Err(e) = ensure_dir(&settings.output_folder) {
        tx.log(format!("ERROR: cannot create output folder: {e:#}"));
        tx.send(WorkerMsg::Finished { ok: 0, failed: total, cancelled: false });
        return;
    }
    let work_root = settings.output_folder.join(".work");
    // Resume-friendly: the work directory is NEVER wiped automatically. It holds
    // the rendered clips and combined batches — by far the most expensive part of
    // the pipeline, and the part SoundCloud rate-limits — so an interrupted run
    // can continue instead of starting from scratch. Valid artifacts are reused
    // below and only missing or corrupt ones are regenerated. The directory is
    // removed only once the final video has been produced successfully (or when
    // the user explicitly chooses "Start over" in the GUI).
    let _ = ensure_dir(&work_root);
    let ffprobe = crate::video::probe::ffprobe_path(&settings.ffmpeg_path);
    let ffmpeg_log = settings.tool_log("ffmpeg.log");

    // Checkpoint: an explicit record of progress (resume.json). It is written as
    // the run advances and read on resume so the state is stated, not inferred;
    // the ffprobe-validated reuse below is still the final word on any artifact.
    let mut checkpoint = Checkpoint::load_or_new(&work_root);
    if checkpoint.has_progress() {
        tx.log(format!(
            "Resuming from checkpoint (stage: {}, {} clip(s), {} batch(es) recorded, encoder {}).",
            if checkpoint.stage.is_empty() { "?" } else { &checkpoint.stage },
            checkpoint.completed_clips.len(),
            checkpoint.completed_batches.len(),
            if checkpoint.encoder.is_empty() { "?" } else { &checkpoint.encoder },
        ));
        if !checkpoint.encoder.is_empty() && checkpoint.encoder != encoder.kind.codec() {
            tx.log(format!(
                "Note: previous run used {}, now using {} — already-finished artifacts are kept; \
                 the final video re-encodes uniformly.",
                checkpoint.encoder,
                encoder.kind.codec()
            ));
        }
    }
    checkpoint.playlist = playlist_name.clone();
    checkpoint.encoder = encoder.kind.codec().to_string();
    checkpoint.set_stage(&work_root, "render");

    let mut clips: Vec<ClipInfo> = Vec::new();
    // Track index -> whether it made it into the final video, for status updates.
    let mut contributing: Vec<usize> = Vec::new();
    let mut failed = 0usize;

    for (n, (index, track)) in tracks.iter().enumerate() {
        if token.is_cancelled() {
            tx.send(WorkerMsg::Finished { ok: 0, failed, cancelled: true });
            return;
        }
        let (index, track) = (*index, track);
        let label = format!("{} - {}", track.title, track.uploader);
        let workdir = work_root.join(format!("track_{index:04}"));
        let clip_path = work_root.join(format!("clip_{index:04}.mp4"));

        // Resume: a clip a previous run already rendered, and which still probes
        // cleanly, is reused as-is — its download and render are skipped entirely.
        // Validity is decided by ffprobe (not mere existence), so a clip a crash
        // truncated is treated as missing and rebuilt below.
        if let Some(duration) =
            reusable_clip(&ffprobe, &clip_path, &token, ffmpeg_log.as_deref()).await
        {
            tx.log(format!(
                "✓ Reusing clip_{index:04}.mp4 — already rendered ({duration:.1}s)"
            ));
            tx.send(WorkerMsg::Progress {
                phase: format!("Reusing rendered clip {}/{total}: {label}", n + 1),
                frac: (n as f32 + 0.5) / (total as f32 + 1.0),
            });
            clips.push(ClipInfo {
                path: clip_path,
                duration,
                title: track.title.clone(),
                uploader: track.uploader.clone(),
            });
            contributing.push(index);
            checkpoint.record_clip(&work_root, index);
            record_render(index, CachedRender::Done, Some(format!("clip_{index:04}.mp4")));
            // Any half-finished download directory for this track is now stale.
            let _ = std::fs::remove_dir_all(&workdir);
            continue;
        }

        let _ = ensure_dir(&workdir);

        tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Downloading));
        tx.send(WorkerMsg::Progress {
            phase: format!("Downloading Track {}/{total}: {label}", n + 1),
            frac: n as f32 / (total as f32 + 1.0),
        });

        let result = async {
            let downloaded =
                download_and_validate(&settings, &track.url, &workdir, &token, &tx).await?;
            tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Rendering));
            tx.send(WorkerMsg::Progress {
                phase: format!("Rendering clip {}/{total}: {label}", n + 1),
                frac: (n as f32 + 0.5) / (total as f32 + 1.0),
            });
            let duration = renderer::render_track_to(
                &settings,
                &downloaded,
                track.play_seconds,
                &clip_path,
                true,
                encoder,
                &workdir,
                &token,
                &tx,
            )
            .await?;
            Ok::<_, anyhow::Error>((duration, downloaded))
        }
        .await;

        match result {
            Ok((duration, downloaded)) => {
                clips.push(ClipInfo {
                    path: clip_path,
                    duration,
                    title: downloaded.meta.title.clone(),
                    uploader: downloaded.meta.uploader.clone(),
                });
                contributing.push(index);
                checkpoint.record_clip(&work_root, index);
                record_render(index, CachedRender::Done, Some(format!("clip_{index:04}.mp4")));
                let _ = std::fs::remove_dir_all(&workdir);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if token.is_cancelled() || msg.contains("cancelled") {
                    tx.log(format!("Cancelled during: {label}"));
                    tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Pending));
                    // Keep .work so the next run resumes from the clips already
                    // rendered instead of downloading them all over again.
                    tx.send(WorkerMsg::Finished { ok: 0, failed, cancelled: true });
                    return;
                }
                failed += 1;
                // Withheld-access failures get the cookies hint appended, so the
                // row explains what might actually help.
                let msg = ytdlp::annotate_download_failure(&msg);
                tx.log(format!("ERROR on '{label}': {msg}"));
                record_render(index, CachedRender::Failed { reason: msg.clone() }, None);
                tx.send(WorkerMsg::TrackStatus(index, TrackStatus::Failed(msg)));
                if !settings.continue_on_fail {
                    tx.log(format!(
                        "Stopping export: 'Continue when a track fails' is off and \
                         track {} failed.",
                        n + 1
                    ));
                    let reason =
                        format!("Failed combining playlist video: track {} render missing", n + 1);
                    for idx in &contributing {
                        tx.send(WorkerMsg::TrackStatus(*idx, TrackStatus::Failed(reason.clone())));
                    }
                    // Keep .work: the clips rendered so far let a later run resume.
                    tx.send(WorkerMsg::Finished { ok: 0, failed, cancelled: false });
                    return;
                }
            }
        }
    }

    if clips.is_empty() {
        tx.log("ERROR: no tracks rendered successfully; nothing to combine.".to_string());
        tx.send(WorkerMsg::Finished { ok: 0, failed, cancelled: false });
        return;
    }

    // ---- Combine ----------------------------------------------------------
    checkpoint.set_stage(&work_root, "combine");
    tx.send(WorkerMsg::Progress {
        phase: format!("Combining {} track(s) into one video...", clips.len()),
        frac: total as f32 / (total as f32 + 1.0),
    });

    let name = {
        let n = settings.playlist_video_name.trim();
        sanitize_filename(if n.is_empty() { &playlist_name } else { n })
    };
    let output = unique_path(&settings.output_folder, &name, "mp4");

    let renderer_job = PlaylistRenderer {
        clips,
        output: output.clone(),
        playlist_title: if playlist_name.trim().is_empty() {
            name.clone()
        } else {
            playlist_name.clone()
        },
        transition: settings.transition_seconds,
        chapters: settings.enable_chapters,
        audio_bitrate_k: settings.bitrate_k(),
        encoder,
        chunk_size: settings.combine_chunk(),
        batch_chunk_size: settings.batch_combine_chunk(),
    };

    let combine_workdir = work_root.clone();
    let _ = ensure_dir(&combine_workdir);
    let combine = renderer_job
        .combine(
            &settings.ffmpeg_path,
            &combine_workdir,
            &token,
            &tx,
            settings.tool_log("ffmpeg.log").as_deref(),
        )
        .await;

    match combine {
        Ok(()) => {
            tx.log(format!("Finished playlist video: {}", output.display()));
            for idx in &contributing {
                tx.send(WorkerMsg::TrackStatus(*idx, TrackStatus::Done(output.clone())));
            }
            tx.send(WorkerMsg::Progress { phase: "Done".into(), frac: 1.0 });
            tx.send(WorkerMsg::Finished {
                ok: contributing.len(),
                failed,
                cancelled: token.is_cancelled(),
            });
            // Success: the final video exists and has fully consumed the clips and
            // batches, so the work directory can be cleaned up. This is the only
            // path that deletes it automatically.
            let _ = std::fs::remove_dir_all(&work_root);
        }
        Err(e) => {
            let msg = format!("{e:#}");
            if token.is_cancelled() || msg.contains("cancelled") {
                tx.send(WorkerMsg::Finished { ok: 0, failed, cancelled: true });
            } else {
                let reason = format!("Failed combining playlist video:\n{msg}");
                tx.log(format!("ERROR: {reason}"));
                tx.log(format!(
                    "All {} track(s) rendered successfully; the failure is in the final \
                     combine step, not in the tracks. The rendered clips and any finished \
                     batches have been kept — press Convert again to resume where this left off.",
                    contributing.len()
                ));
                // These tracks did NOT fail — their clips rendered fine and are
                // kept for the resume. Mark them Pending (they will be picked up
                // again) rather than Failed, and do not count them as failures:
                // only the single combine step failed. Counting them here is what
                // used to turn one combine error into a misleading "N tracks
                // failed" summary.
                for idx in &contributing {
                    tx.send(WorkerMsg::TrackStatus(*idx, TrackStatus::Pending));
                }
                tx.send(WorkerMsg::Finished { ok: 0, failed, cancelled: false });
            }
            // Failure or cancellation: keep .work so the next run can resume.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_refresh_fires_only_once_and_only_with_a_browser() {
        // Happy path: enabled, not yet refreshed, a browser is configured.
        assert!(should_refresh_cookies(true, false, Some(CookieBrowser::Firefox)));
        // Already refreshed once — never a second time (the "one refresh per
        // request" rule).
        assert!(!should_refresh_cookies(true, true, Some(CookieBrowser::Firefox)));
        // Feature disabled.
        assert!(!should_refresh_cookies(false, false, Some(CookieBrowser::Firefox)));
        // No browser to refresh from.
        assert!(!should_refresh_cookies(true, false, None));
    }

    #[test]
    fn only_restricted_failures_map_to_a_cookie_refresh() {
        // The retry engine keys off RetryDecision; restricted → refresh, the
        // others never do.
        assert_eq!(
            ytdlp::retry_decision("ERROR: This video is DRM protected"),
            RetryDecision::RefreshCookiesAndRetry
        );
        assert_eq!(
            ytdlp::retry_decision("ERROR: HTTP Error 429: Too Many Requests"),
            RetryDecision::Retry
        );
        assert_eq!(
            ytdlp::retry_decision("ERROR: HTTP Error 404: Not Found"),
            RetryDecision::PermanentFailure
        );
    }

    #[test]
    fn metafetch_uses_the_settings_exponential_ladder_and_browser() {
        let mut s = Settings::default();
        s.cookie_browser = CookieBrowser::Firefox;
        let m = MetaFetch::new(&s, Duration::ZERO, s.metadata_retry_attempts());
        assert_eq!(m.browser, Some(CookieBrowser::Firefox));
        assert_eq!(m.max_attempts, 6);
        // The shared gate carries the exponential ladder from settings.
        let ladder: Vec<u64> = m.gate.steps.iter().map(|d| d.as_secs()).collect();
        assert_eq!(ladder, vec![5, 10, 20, 40, 60, 60]);
    }

    #[test]
    fn metafetch_without_a_browser_cannot_refresh() {
        let s = Settings::default(); // cookie_browser defaults to None
        let m = MetaFetch::new(&s, Duration::ZERO, 1);
        assert_eq!(m.browser, None);
        assert!(!should_refresh_cookies(m.auto_cookie_refresh, false, m.browser));
        // max_attempts is floored at 1 even if a caller passes 0.
        let m0 = MetaFetch::new(&s, Duration::ZERO, 0);
        assert_eq!(m0.max_attempts, 1);
    }
}
