use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use egui::{Color32, RichText};
use tokio_util::sync::CancellationToken;

use super::components::{self, track_row};
use crate::config::settings::{
    ExportMode, Settings, BATCH_COMBINE_CHUNK_PRESETS, COMBINE_CHUNK_PRESETS,
    METADATA_RETRY_ATTEMPTS_RANGE, QUALITIES, RESOLUTIONS, RETRY_CONCURRENCY_RANGE,
    RETRY_DELAY_MS_RANGE, RETRY_INITIAL_DELAY_RANGE, RETRY_MAX_DELAY_RANGE,
};
use crate::downloader::ytdlp::FlatEntry;
use crate::models::messages::{BenchmarkResult, Tx, WorkerMsg};
use crate::models::track::{MetaFailure, MetaState, Track, TrackStatus};
use crate::video::encoder::{EncoderChoice, EncoderSupport};
use crate::{pipeline, setup};

/// First wait between automatic recovery passes. Short, because the first pass
/// often clears most tracks and a quick second pass mops up the rest.
const RECOVERY_BACKOFF_START: Duration = Duration::from_secs(15);
/// Cap on the recovery back-off. A sustained rate limit settles into one quiet
/// pass every five minutes rather than spinning — but it never gives up.
const RECOVERY_BACKOFF_MAX: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq)]
enum AppState {
    Idle,
    LoadingPlaylist,
    Converting,
}

/// State of the ffmpeg / yt-dlp setup assistant dialog.
enum SetupUi {
    Hidden,
    /// Tools missing and winget is available — offer automatic install.
    Prompt,
    /// Tools missing but winget is unavailable — show manual instructions.
    NoWinget,
    /// Installation in progress; holds the streamed output lines.
    Installing(Vec<String>),
    Failed { message: String, needs_elevation: bool },
    Success(String),
}

/// Deferred action from the setup dialog, applied after its UI is drawn.
enum SetupAction {
    None,
    Install,
    Dismiss,
    Elevate,
    Close,
}

#[derive(Default)]
struct ToolStatus {
    checked: bool,
    ffmpeg: Option<String>,
    ytdlp: Option<String>,
}

struct ProgressInfo {
    phase: String,
    frac: f32,
}

/// Live state of a metadata retry pass.
struct RetryProgress {
    remaining: usize,
    total: usize,
    /// Started by the app after a rate-limited load, not by the user.
    auto: bool,
}

/// Result of the last finished retry pass, shown until the next one starts.
struct RetrySummary {
    recovered: usize,
    still_failed: usize,
    /// Of `still_failed`, how many are gone for good (deleted, private,
    /// region-locked). These are reported, not offered for retry again.
    permanent: usize,
    /// Of `still_failed`, how many SoundCloud refused anonymously. Not retried
    /// automatically, but a cookies file may get through.
    restricted: usize,
    cancelled: bool,
    /// The pass stopped early because the rate limit never eased.
    gave_up: bool,
    auto: bool,
}

pub struct App {
    rt: tokio::runtime::Runtime,
    tx: Tx,
    rx: Receiver<WorkerMsg>,

    settings: Settings,
    url: String,
    playlist_title: Option<String>,
    tracks: Vec<Track>,

    state: AppState,
    cancel: Option<CancellationToken>,
    /// Cancels playlist loading + background metadata enrichment.
    meta_cancel: Option<CancellationToken>,
    /// True while the initial metadata enrichment pass is still running; the
    /// retry button only appears once it has finished.
    meta_loading: bool,
    /// Cancels an in-flight retry pass (separate from `meta_cancel` so
    /// cancelling a retry never disturbs anything else).
    retry_cancel: Option<CancellationToken>,
    retry_progress: Option<RetryProgress>,
    retry_summary: Option<RetrySummary>,
    /// Whether the in-flight (or just-finished) retry was started automatically.
    retry_was_automatic: bool,
    /// When the next automatic recovery pass is due, while waiting out the
    /// inter-pass back-off. `None` when recovery is not waiting.
    recovery_next_at: Option<Instant>,
    /// Current inter-pass back-off for the continuous recovery loop; grows
    /// (capped) after a fruitless pass and resets when a pass recovers anything.
    recovery_backoff: Duration,
    /// Failure each retried row had before the retry started, so a cancelled
    /// pass can put rows it never reached back the way it found them.
    retry_prev_errors: std::collections::HashMap<usize, MetaFailure>,
    /// Last cookie probe result, or None if never checked this session.
    cookie_status: Option<crate::downloader::cookies::CookieStatus>,
    /// A probe is in flight.
    cookie_checking: bool,
    progress: Option<ProgressInfo>,
    tools: ToolStatus,
    error: Option<String>,
    logs: Vec<String>,
    show_logs: bool,
    /// Export mode captured when the current conversion started, so the
    /// completion message matches what actually ran.
    active_export_mode: ExportMode,
    setup: SetupUi,
    /// True once the user has dismissed the setup dialog this session.
    setup_dismissed: bool,
    /// True while the "resume or start over?" dialog is up — shown when a
    /// Combined export is started against a `.work` folder that still holds
    /// artifacts from an interrupted run.
    resume_prompt: bool,
    /// Detected encoder support (which of NVENC/QSV/AMF actually work here).
    /// `None` until the startup probe reports back.
    encoder_support: Option<EncoderSupport>,
    /// Label of the encoder the running conversion is using, for the progress
    /// display (e.g. "AMD AMF (h264_amf)").
    active_encoder: Option<String>,
    /// A benchmark render is in flight.
    benchmarking: bool,
    /// Recent benchmark results, newest first, for comparing encoders.
    benchmarks: Vec<BenchmarkResult>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        let (tx_raw, rx) = channel();
        let tx = Tx { tx: tx_raw, ctx: cc.egui_ctx.clone() };

        let settings = Settings::load();
        let settings_export_mode = settings.export_mode;

        // Probe for ffmpeg / yt-dlp in the background.
        rt.spawn(pipeline::check_tools(
            settings.ffmpeg_path.clone(),
            settings.ytdlp_path.clone(),
            tx.clone(),
        ));
        // Probe which video encoders this machine can actually use.
        rt.spawn(pipeline::detect_encoders(settings.ffmpeg_path.clone(), tx.clone()));

        Self {
            rt,
            tx,
            rx,
            settings,
            url: String::new(),
            playlist_title: None,
            tracks: Vec::new(),
            state: AppState::Idle,
            cancel: None,
            meta_cancel: None,
            meta_loading: false,
            retry_cancel: None,
            retry_progress: None,
            retry_summary: None,
            retry_was_automatic: false,
            recovery_next_at: None,
            recovery_backoff: RECOVERY_BACKOFF_START,
            retry_prev_errors: std::collections::HashMap::new(),
            cookie_status: None,
            cookie_checking: false,
            progress: None,
            tools: ToolStatus::default(),
            error: None,
            logs: Vec::new(),
            show_logs: false,
            active_export_mode: settings_export_mode,
            setup: SetupUi::Hidden,
            setup_dismissed: false,
            resume_prompt: false,
            // Show the last known encoder support instantly; a background probe
            // refreshes it within a second of launch.
            encoder_support: crate::video::encoder::load_cache(),
            active_encoder: None,
            benchmarking: false,
            benchmarks: Vec::new(),
        }
    }

    /// Resolve the configured encoder against what the machine actually
    /// supports. Any fallback note is surfaced in the Encoding settings row; the
    /// conversion also logs the active encoder via `WorkerMsg::EncoderActive`.
    fn resolve_encoder(&self) -> crate::video::encoder::ResolvedEncoder {
        let support = self.encoder_support.clone().unwrap_or_default();
        support
            .resolve(self.settings.encoder, self.settings.hardware_decode)
            .0
    }

    fn busy(&self) -> bool {
        self.state != AppState::Idle
    }

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Log(line) => {
                    if self.settings.debug_mode {
                        crate::utils::process::append_log(
                            &Settings::logs_dir().join("app.log"),
                            &line,
                        );
                    }
                    self.logs.push(line);
                    if self.logs.len() > 3000 {
                        self.logs.drain(..1000);
                    }
                }
                WorkerMsg::Playlist(result) => {
                    self.state = AppState::Idle;
                    match result {
                        Ok((title, tracks)) => {
                            self.playlist_title = Some(title);
                            self.tracks = tracks;
                            self.error = None;
                            self.announce_resume_if_any();
                        }
                        Err(e) => {
                            self.error = Some(e);
                            // No enrichment pass will run, so don't leave the
                            // UI waiting for MetaLoadFinished.
                            self.meta_loading = false;
                        }
                    }
                }
                WorkerMsg::TrackMeta(index, result) => {
                    let global_max = self.settings.max_track_seconds;
                    if let Some(t) = self.tracks.get_mut(index) {
                        match result {
                            Ok(meta) => t.apply_metadata(&meta, global_max),
                            Err(failure) => {
                                // A track that can never load can never be
                                // converted either. Leaving it selected would
                                // fail the download mid-export — and with
                                // "continue when a track fails" off, abort the
                                // whole run. The user can re-tick it if they
                                // disagree.
                                if failure.is_permanent() {
                                    t.selected = false;
                                }
                                t.meta = MetaState::Failed(failure);
                            }
                        }
                    }
                }
                WorkerMsg::MetaLoadFinished {
                    cancelled,
                    failed,
                    rate_limited: _,
                    permanent,
                    restricted,
                } => {
                    self.meta_loading = false;
                    if permanent > 0 {
                        self.logs.push(format!(
                            "{permanent} of {failed} failed track(s) are permanently \
                             unavailable (deleted, private or region-locked) — they are \
                             marked as such and will not be retried."
                        ));
                    }
                    if restricted > 0 {
                        self.logs.push(format!(
                            "{restricted} of {failed} failed track(s) were refused to an \
                             anonymous client. These are not necessarily gone — set a \
                             cookies file under Settings → SoundCloud Authentication and press Retry \
                             Failed Metadata to try again signed in."
                        ));
                    }
                    // Recover everything still missing in the background, without
                    // the user clicking anything — automatic recovery keeps
                    // re-trying (with back-off) until the whole playlist resolves.
                    if !cancelled {
                        self.maybe_start_recovery();
                    }
                }
                WorkerMsg::MetaRetrying(index) => {
                    if let Some(t) = self.tracks.get_mut(index) {
                        t.meta = MetaState::Loading;
                    }
                }
                WorkerMsg::MetaRetryProgress { remaining, total } => {
                    self.retry_progress = Some(RetryProgress {
                        remaining,
                        total,
                        auto: self.retry_was_automatic,
                    });
                }
                WorkerMsg::MetaRetryFinished {
                    recovered,
                    still_failed,
                    permanent,
                    restricted,
                    cancelled,
                    gave_up,
                } => {
                    self.retry_cancel = None;
                    self.retry_progress = None;
                    self.retry_summary = Some(RetrySummary {
                        recovered,
                        still_failed,
                        permanent,
                        restricted,
                        cancelled,
                        gave_up,
                        auto: self.retry_was_automatic,
                    });
                    // A cancelled retry leaves rows it never got to in the
                    // "fetching metadata..." state; restore their last error.
                    self.restore_unfinished_retries();
                    // Keep the background recovery loop going until every track
                    // resolves (or the user cancels) — this is what removes the
                    // repeated-button-press workflow.
                    self.rearm_recovery(recovered, cancelled);
                }
                WorkerMsg::TrackStatus(index, status) => {
                    if let Some(t) = self.tracks.get_mut(index) {
                        t.status = status;
                    }
                }
                WorkerMsg::Progress { phase, frac } => {
                    self.progress = Some(ProgressInfo { phase, frac });
                }
                WorkerMsg::Finished { ok, failed, cancelled } => {
                    self.state = AppState::Idle;
                    self.cancel = None;
                    self.active_encoder = None;
                    let summary = self.completion_message(ok, failed, cancelled);
                    self.logs.push(summary.clone());
                    self.progress = Some(ProgressInfo {
                        phase: summary,
                        frac: if cancelled { 0.0 } else { 1.0 },
                    });
                }
                WorkerMsg::Tools { ffmpeg, ytdlp } => {
                    self.tools = ToolStatus { checked: true, ffmpeg, ytdlp };
                    self.maybe_open_setup();
                }
                WorkerMsg::Encoders(support) => {
                    self.encoder_support = Some(support);
                }
                WorkerMsg::EncoderActive(label) => {
                    self.active_encoder = Some(label);
                }
                WorkerMsg::BenchmarkDone(result) => {
                    self.benchmarking = false;
                    match result {
                        Ok(r) => self.benchmarks.insert(0, r),
                        Err(e) => self.error = Some(format!("Benchmark failed: {e}")),
                    }
                    self.benchmarks.truncate(6);
                }
                WorkerMsg::CookieStatus(status) => {
                    self.cookie_checking = false;
                    self.logs.push(format!(
                        "Cookie check ({}): {}",
                        self.settings.cookie_browser.label(),
                        status.headline()
                    ));
                    self.cookie_status = Some(status);
                }
                WorkerMsg::SetupProgress(line) => {
                    self.logs.push(line.clone());
                    if let SetupUi::Installing(log) = &mut self.setup {
                        log.push(line);
                        if log.len() > 400 {
                            log.drain(..200);
                        }
                    }
                }
                WorkerMsg::SetupDone { success, message, needs_elevation } => {
                    self.logs.push(message.clone());
                    self.setup = if success {
                        SetupUi::Success(message)
                    } else {
                        SetupUi::Failed { message, needs_elevation }
                    };
                }
            }
        }
    }

    fn retrying(&self) -> bool {
        self.retry_cancel.is_some()
    }

    /// Rows the **manual** retry button would act on: everything that failed
    /// and is not permanently gone, including access-restricted tracks — the
    /// user may have added a cookies file since the last attempt.
    ///
    /// Permanently unavailable tracks are excluded: they are the reason a
    /// handful of rows used to sit in the queue forever, failing identically.
    fn failed_meta_count(&self) -> usize {
        self.tracks
            .iter()
            .filter(|t| matches!(&t.meta, MetaState::Failed(f) if !f.is_permanent()))
            .count()
    }

    /// Rows that can never load, however many times they are retried.
    fn permanent_meta_count(&self) -> usize {
        self.tracks
            .iter()
            .filter(|t| matches!(&t.meta, MetaState::Failed(f) if f.is_permanent()))
            .count()
    }

    /// Rows SoundCloud refused anonymously — recoverable in principle.
    fn restricted_meta_count(&self) -> usize {
        self.tracks
            .iter()
            .filter(|t| matches!(&t.meta, MetaState::Failed(f) if f.is_restricted()))
            .count()
    }

    /// Start a retry for every row that currently shows a retryable metadata
    /// error. Rows with valid metadata are not in the list, so they are never
    /// re-fetched and never overwritten; neither are rows already known to be
    /// permanently unavailable.
    ///
    /// `auto` (the pass that fires itself after a rate-limited load) takes only
    /// failures that a plain repeat could fix. The manual button additionally
    /// takes access-restricted rows, because pressing it is a deliberate act
    /// that usually follows configuring cookies.
    fn start_retry_metadata(&mut self, auto: bool) {
        let mut targets: Vec<(usize, FlatEntry)> = Vec::new();
        self.retry_prev_errors.clear();
        for (i, t) in self.tracks.iter().enumerate() {
            let MetaState::Failed(failure) = &t.meta else { continue };
            if failure.is_permanent() || (auto && !failure.is_auto_retryable()) {
                continue;
            }
            self.retry_prev_errors.insert(i, failure.clone());
            targets.push((
                i,
                FlatEntry { id: t.id.clone(), url: t.url.clone(), title: None },
            ));
        }
        if targets.is_empty() {
            return;
        }
        self.retry_summary = None;
        self.retry_was_automatic = auto;
        self.retry_progress = Some(RetryProgress {
            remaining: targets.len(),
            total: targets.len(),
            auto,
        });

        let token = CancellationToken::new();
        self.retry_cancel = Some(token.clone());
        self.rt.spawn(pipeline::retry_failed_metadata(
            self.settings.clone(),
            self.url.clone(),
            targets,
            self.tx.clone(),
            token,
        ));
    }

    fn cancel_retry(&mut self) {
        // Cancelling stops the whole background recovery loop, not just the pass
        // in flight — a pending re-arm is dropped too.
        self.recovery_next_at = None;
        if let Some(token) = &self.retry_cancel {
            token.cancel();
            self.logs.push("Stopping metadata recovery...".into());
        }
    }

    /// Tracks the automatic recovery loop can still make progress on: retryable
    /// metadata failures (not permanent, not access-restricted).
    fn auto_recoverable_count(&self) -> usize {
        self.tracks
            .iter()
            .filter(|t| matches!(&t.meta, MetaState::Failed(f) if f.is_auto_retryable()))
            .count()
    }

    /// Tracks with fully-resolved metadata — the "recovered" numerator in the
    /// recovery progress display.
    fn resolved_meta_count(&self) -> usize {
        self.tracks
            .iter()
            .filter(|t| matches!(t.meta, MetaState::Loaded))
            .count()
    }

    /// Start a recovery pass now if automatic recovery is enabled and there is
    /// anything to recover. Safe to call whenever the metadata picture changes.
    fn maybe_start_recovery(&mut self) {
        if !self.settings.auto_recover_metadata
            || self.retrying()
            || self.meta_loading
            || self.state == AppState::Converting
            || self.recovery_next_at.is_some()
            || self.auto_recoverable_count() == 0
        {
            return;
        }
        self.logs.push(format!(
            "→ Recovering missing metadata automatically ({} track(s) remaining)",
            self.auto_recoverable_count()
        ));
        self.start_retry_metadata(true);
    }

    /// After a recovery pass finishes, either declare recovery complete or
    /// schedule the next pass behind an exponential back-off — so the user never
    /// has to press the button again. A cancelled pass, or recovery being turned
    /// off, ends the loop.
    fn rearm_recovery(&mut self, recovered: usize, cancelled: bool) {
        if !self.settings.auto_recover_metadata || cancelled {
            self.recovery_next_at = None;
            return;
        }
        let remaining = self.auto_recoverable_count();
        if remaining == 0 {
            self.recovery_next_at = None;
            self.recovery_backoff = RECOVERY_BACKOFF_START;
            self.logs
                .push("✓ Metadata recovery complete — every track resolved.".into());
            return;
        }
        // Progress resets the back-off; a fruitless pass doubles it (capped), so
        // a hard rate limit settles into occasional quiet passes.
        self.recovery_backoff = if recovered > 0 {
            RECOVERY_BACKOFF_START
        } else {
            (self.recovery_backoff * 2).min(RECOVERY_BACKOFF_MAX)
        };
        self.recovery_next_at = Some(Instant::now() + self.recovery_backoff);
        self.logs.push(format!(
            "⏳ {remaining} track(s) still missing metadata — retrying in {}s.",
            self.recovery_backoff.as_secs()
        ));
    }

    /// Fire the next scheduled recovery pass once its back-off elapses, and keep
    /// the countdown repainting while it waits. Called every frame.
    fn tick_recovery(&mut self, ctx: &egui::Context) {
        let Some(at) = self.recovery_next_at else { return };
        if Instant::now() >= at {
            self.recovery_next_at = None;
            if self.state != AppState::Converting && !self.retrying() {
                self.start_retry_metadata(true);
            }
        } else {
            // Keep the "retrying in Ns" countdown live.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Put rows a cancelled retry never finished back to their prior error, so
    /// no row is left stuck on "fetching metadata...".
    fn restore_unfinished_retries(&mut self) {
        for (index, failure) in &self.retry_prev_errors {
            if let Some(t) = self.tracks.get_mut(*index) {
                if matches!(t.meta, MetaState::Loading) {
                    t.meta = MetaState::Failed(failure.clone());
                }
            }
        }
        self.retry_prev_errors.clear();
    }

    fn start_load_playlist(&mut self) {
        // Stop any still-running metadata fetches from a previous playlist.
        if let Some(old) = self.meta_cancel.take() {
            old.cancel();
        }
        if let Some(old) = self.retry_cancel.take() {
            old.cancel();
        }
        self.retry_progress = None;
        self.retry_summary = None;
        self.retry_prev_errors.clear();
        self.recovery_next_at = None;
        self.recovery_backoff = RECOVERY_BACKOFF_START;
        self.meta_loading = true;
        self.error = None;
        self.playlist_title = None;
        self.tracks.clear();
        self.state = AppState::LoadingPlaylist;
        let token = CancellationToken::new();
        self.meta_cancel = Some(token.clone());
        let _ = self.settings.save();
        self.rt.spawn(pipeline::load_playlist(
            self.settings.clone(),
            self.url.clone(),
            self.tx.clone(),
            token,
        ));
    }

    fn start_convert(&mut self) {
        let selected: Vec<(usize, Track)> = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.selected)
            .map(|(i, t)| (i, t.clone()))
            .collect();
        if selected.is_empty() {
            self.error = Some("No tracks selected.".into());
            return;
        }
        self.error = None;
        for (_, t) in self.tracks.iter_mut().enumerate() {
            if t.selected {
                t.status = TrackStatus::Pending;
            }
        }
        self.state = AppState::Converting;
        self.active_export_mode = self.settings.export_mode;
        self.progress = Some(ProgressInfo { phase: "Starting...".into(), frac: 0.0 });
        let token = CancellationToken::new();
        self.cancel = Some(token.clone());
        let _ = self.settings.save();
        let encoder = self.resolve_encoder();
        match self.settings.export_mode {
            ExportMode::Separate => {
                self.rt.spawn(pipeline::convert(
                    self.settings.clone(),
                    selected,
                    encoder,
                    self.tx.clone(),
                    token,
                ));
            }
            ExportMode::Combined => {
                let playlist_name = self.playlist_title.clone().unwrap_or_default();
                self.rt.spawn(pipeline::convert_combined(
                    self.settings.clone(),
                    playlist_name,
                    self.url.clone(),
                    selected,
                    encoder,
                    self.tx.clone(),
                    token,
                ));
            }
        }
    }

    /// Convert-button entry point. A Combined export whose `.work` folder still
    /// holds artifacts from an interrupted run first asks whether to resume or
    /// start over; everything else starts immediately.
    fn on_convert_clicked(&mut self) {
        if self.settings.export_mode == ExportMode::Combined
            && Self::work_has_resumable_artifacts(&self.settings.output_folder.join(".work"))
        {
            self.resume_prompt = true;
            return;
        }
        self.start_convert();
    }

    /// After a playlist loads, tell the user if an interrupted conversion is
    /// waiting to be resumed. Metadata recovery (started separately) fills in the
    /// rest in the background while they decide whether to press Convert/Resume.
    fn announce_resume_if_any(&mut self) {
        let work_root = self.settings.output_folder.join(".work");
        if !Self::work_has_resumable_artifacts(&work_root) {
            return;
        }
        let clips = std::fs::read_dir(&work_root)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| {
                        let n = e.file_name();
                        let n = n.to_string_lossy();
                        n.starts_with("clip_") && n.ends_with(".mp4")
                    })
                    .count()
            })
            .unwrap_or(0);
        self.logs.push("Found a previous conversion in progress.".into());
        if clips > 0 {
            self.logs
                .push(format!("{clips} rendered clip(s) found — they will be reused on Resume."));
        }
        self.logs
            .push("Press Convert to resume where it left off; metadata recovery continues in the background.".into());
    }

    /// Whether `.work` holds anything a Combined export could reuse: at least one
    /// rendered clip or combined batch left by a previous run.
    fn work_has_resumable_artifacts(work_root: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir(work_root) else {
            return false;
        };
        entries.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            (name.starts_with("clip_") && name.ends_with(".mp4"))
                || (name.starts_with("batch_") && name.ends_with(".mkv"))
        })
    }

    fn cancel_work(&mut self) {
        if let Some(token) = &self.cancel {
            token.cancel();
        }
        if self.state == AppState::LoadingPlaylist {
            if let Some(token) = self.meta_cancel.take() {
                token.cancel();
            }
            self.state = AppState::Idle;
            self.meta_loading = false;
        }
        self.cancel_retry();
        self.logs.push("Cancelling...".into());
    }

    fn recheck_tools(&mut self) {
        self.tools.checked = false;
        self.rt.spawn(pipeline::check_tools(
            self.settings.ffmpeg_path.clone(),
            self.settings.ytdlp_path.clone(),
            self.tx.clone(),
        ));
        // A different ffmpeg may support a different set of encoders.
        self.encoder_support = None;
        self.rt.spawn(pipeline::detect_encoders(
            self.settings.ffmpeg_path.clone(),
            self.tx.clone(),
        ));
    }

    /// Kick off a 30-second benchmark of the currently selected encoder.
    fn start_benchmark(&mut self) {
        if self.benchmarking || self.busy() {
            return;
        }
        self.benchmarking = true;
        self.error = None;
        let encoder = self.resolve_encoder();
        let token = CancellationToken::new();
        self.rt.spawn(pipeline::benchmark_encoder(
            self.settings.clone(),
            encoder,
            self.tx.clone(),
            token,
        ));
    }

    /// Completion summary phrased for the export mode that actually ran.
    fn completion_message(&self, ok: usize, failed: usize, cancelled: bool) -> String {
        match self.active_export_mode {
            ExportMode::Separate => {
                if cancelled {
                    format!("Cancelled — {ok} video(s) created, {failed} failed")
                } else if failed > 0 {
                    format!("Done — {ok} video(s) created, {failed} failed")
                } else {
                    format!("Done — {ok} video(s) created 🎉")
                }
            }
            ExportMode::Combined => {
                if cancelled {
                    "Cancelled — no playlist video created".to_string()
                } else if ok == 0 {
                    format!("Failed — no playlist video created ({failed} track(s) failed)")
                } else if failed > 0 {
                    format!("Done — 1 playlist video created ({failed} track(s) skipped) 🎉")
                } else {
                    "Done — 1 playlist video created 🎉".to_string()
                }
            }
        }
    }

    /// Open the setup assistant once at startup if a tool is missing.
    fn maybe_open_setup(&mut self) {
        if self.setup_dismissed || !matches!(self.setup, SetupUi::Hidden) || !self.tools.checked {
            return;
        }
        let missing = self.tools.ffmpeg.is_none() || self.tools.ytdlp.is_none();
        if !missing {
            return;
        }
        self.setup = if setup::winget_available() {
            SetupUi::Prompt
        } else {
            SetupUi::NoWinget
        };
    }

    fn start_install(&mut self) {
        let install_ffmpeg = self.tools.ffmpeg.is_none();
        let install_ytdlp = self.tools.ytdlp.is_none();
        self.setup = SetupUi::Installing(vec!["Starting installation...".into()]);
        self.rt.spawn(setup::install_missing(
            install_ffmpeg,
            install_ytdlp,
            self.settings.ffmpeg_path.clone(),
            self.settings.ytdlp_path.clone(),
            self.tx.clone(),
        ));
    }

    // ---------------------------------------------------------------- UI ----

    fn tool_badge(ui: &mut egui::Ui, name: &str, version: &Option<String>, checked: bool) {
        let (text, color) = if !checked {
            (format!("{name}: checking..."), Color32::GRAY)
        } else {
            match version {
                Some(_) => (format!("{name} ✓"), components::OK_GREEN),
                None => (format!("{name} ✗ not found"), components::ERR_RED),
            }
        };
        let resp = ui.label(RichText::new(text).color(color).size(12.5));
        if let Some(v) = version {
            resp.on_hover_text(v);
        } else if checked {
            resp.on_hover_text(format!(
                "'{name}' was not found on PATH.\nInstall it or set its full path in Settings below."
            ));
        }
    }

    fn ui_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("SoundCloud -> MP4")
                    .color(components::ACCENT)
                    .strong()
                    .size(20.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⟳ recheck").clicked() {
                    self.recheck_tools();
                }
                Self::tool_badge(ui, "yt-dlp", &self.tools.ytdlp, self.tools.checked);
                Self::tool_badge(ui, "ffmpeg", &self.tools.ffmpeg, self.tools.checked);
            });
        });
    }

    fn ui_input_row(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        ui.horizontal(|ui| {
            ui.label("SoundCloud Playlist URL:");
            let width = (ui.available_width() - 130.0).max(200.0);
            let edit = egui::TextEdit::singleline(&mut self.url)
                .hint_text("https://soundcloud.com/artist/sets/playlist-name")
                .desired_width(width);
            let response = ui.add_enabled(!busy, edit);
            let enter =
                response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui
                .add_enabled(
                    !busy && !self.url.trim().is_empty(),
                    egui::Button::new("Load Playlist"),
                )
                .clicked();
            if (clicked || enter) && !busy && !self.url.trim().is_empty() {
                self.start_load_playlist();
            }
        });
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        let mut tools_changed = false;

        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.label("Audio Quality:");
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(format!("{} kbps", self.settings.default_quality))
                    .show_ui(ui, |ui| {
                        for q in QUALITIES {
                            ui.selectable_value(
                                &mut self.settings.default_quality,
                                q.to_string(),
                                format!("{q} kbps"),
                            );
                        }
                    });

                ui.separator();
                ui.label("Resolution:");
                egui::ComboBox::from_id_salt("resolution")
                    .selected_text(&self.settings.default_resolution)
                    .show_ui(ui, |ui| {
                        for r in RESOLUTIONS {
                            ui.selectable_value(
                                &mut self.settings.default_resolution,
                                r.to_string(),
                                r,
                            );
                        }
                    });

                ui.separator();
                ui.label("Max per song:");
                let mut unlimited = self.settings.max_track_seconds <= 0.0;
                if ui.checkbox(&mut unlimited, "full length").changed() {
                    self.settings.max_track_seconds = if unlimited { 0.0 } else { 60.0 };
                    self.apply_global_max();
                }
                if !unlimited
                    && ui
                        .add(
                            egui::DragValue::new(&mut self.settings.max_track_seconds)
                                .range(5.0..=7200.0)
                                .speed(1.0)
                                .suffix(" s"),
                        )
                        .on_hover_text(
                            "Upper limit for every video. Individual tracks can be set \
                             shorter in the list below.",
                        )
                        .changed()
                {
                    self.apply_global_max();
                }

                ui.separator();
                ui.checkbox(&mut self.settings.effect_fade, "Fade in/out");
                ui.checkbox(&mut self.settings.effect_zoom, "Slow zoom")
                    .on_hover_text("Ken Burns style zoom on the cover (slower to render)");

                ui.separator();
                if ui
                    .checkbox(
                        &mut self.settings.auto_recover_metadata,
                        "Automatically recover missing metadata",
                    )
                    .on_hover_text(
                        "Keep re-fetching any track whose metadata is missing — backing off when \
                         rate limited and continuing until the whole playlist resolves — so you \
                         never have to press Retry Failed Metadata repeatedly. When off, only the \
                         manual button runs.",
                    )
                    .changed()
                {
                    let _ = self.settings.save();
                    // Turning it on with tracks already waiting starts recovery now.
                    if self.settings.auto_recover_metadata {
                        self.maybe_start_recovery();
                    } else {
                        self.recovery_next_at = None;
                    }
                }

                ui.separator();
                if ui
                    .checkbox(&mut self.settings.debug_mode, "Enable debug mode")
                    .on_hover_text(format!(
                        "Writes app.log, yt-dlp.log and ffmpeg.log to\n{}",
                        Settings::logs_dir().display()
                    ))
                    .changed()
                {
                    let _ = self.settings.save();
                }
            });
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.label("Output folder:");
                ui.label(
                    RichText::new(self.settings.output_folder.display().to_string())
                        .weak()
                        .size(12.5),
                );
                if ui.button("Browse...").clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_directory(&self.settings.output_folder)
                        .pick_folder()
                    {
                        self.settings.output_folder = dir;
                        let _ = self.settings.save();
                    }
                }
                if ui.button("Open").clicked() {
                    let _ = std::fs::create_dir_all(&self.settings.output_folder);
                    let _ = open::that(&self.settings.output_folder);
                }
            });
        });

        ui.add_space(4.0);
        self.ui_export_mode(ui, busy);

        ui.add_space(2.0);
        self.ui_encoding(ui, busy);

        ui.add_space(2.0);
        egui::CollapsingHeader::new("Metadata retry")
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if ui
                        .checkbox(
                            &mut self.settings.auto_metadata_retry,
                            "Automatically retry transient failures",
                        )
                        .on_hover_text(
                            "Treat metadata loading like a resilient downloader: on HTTP 429, \
                             5xx or a network error, pause the whole batch on a shared cooldown \
                             and retry with exponential back-off instead of skipping the track. \
                             When off, each track is fetched exactly once.",
                        )
                        .changed()
                    {
                        let _ = self.settings.save();
                    }
                    if ui
                        .checkbox(
                            &mut self.settings.auto_cookie_refresh,
                            "Automatically refresh browser cookies on auth errors",
                        )
                        .on_hover_text(
                            "On a DRM/restricted/authentication error, re-read the selected \
                             browser's cookies and retry the request once before giving up. \
                             No effect when no browser is selected.",
                        )
                        .changed()
                    {
                        let _ = self.settings.save();
                    }

                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label("Max attempts:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.settings.metadata_retry_max_attempts)
                                    .range(METADATA_RETRY_ATTEMPTS_RANGE)
                                    .speed(0.1),
                            )
                            .on_hover_text("Fetch attempts per track, including the first.")
                            .changed()
                        {
                            let _ = self.settings.save();
                        }
                        ui.separator();
                        ui.label("Initial delay:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.settings.retry_initial_delay_secs)
                                    .range(RETRY_INITIAL_DELAY_RANGE)
                                    .speed(0.2)
                                    .suffix(" s"),
                            )
                            .on_hover_text("First rung of the exponential back-off.")
                            .changed()
                        {
                            let _ = self.settings.save();
                        }
                        ui.separator();
                        ui.label("Max delay:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.settings.retry_max_delay_secs)
                                    .range(RETRY_MAX_DELAY_RANGE)
                                    .speed(1.0)
                                    .suffix(" s"),
                            )
                            .on_hover_text("The doubling back-off never exceeds this.")
                            .changed()
                        {
                            let _ = self.settings.save();
                        }
                    });

                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label("Retry-pass delay between requests:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.settings.retry_delay_ms)
                                    .range(RETRY_DELAY_MS_RANGE)
                                    .speed(25.0)
                                    .suffix(" ms"),
                            )
                            .on_hover_text(
                                "Extra pause before each request in the on-demand retry pass \
                                 (gentler than the initial load).",
                            )
                            .changed()
                        {
                            let _ = self.settings.save();
                        }
                        ui.separator();
                        ui.label("Parallel retries:");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.settings.retry_concurrency)
                                    .range(RETRY_CONCURRENCY_RANGE)
                                    .speed(0.1),
                            )
                            .on_hover_text(
                                "How many retries run at once. Kept low on purpose — retries \
                                 must not be more aggressive than the initial load.",
                            )
                            .changed()
                        {
                            let _ = self.settings.save();
                        }
                    });

                    ui.label(
                        RichText::new(format!(
                            "Shared rate-limit cooldown: {}  (one cooldown pauses every worker)",
                            self.settings
                                .retry_backoff()
                                .iter()
                                .map(|d| format!("{}s", d.as_secs()))
                                .collect::<Vec<_>>()
                                .join(" → ")
                        ))
                        .size(12.0)
                        .weak(),
                    )
                    .on_hover_text(
                        "When SoundCloud rate limits, every worker pauses for the next rung of \
                         this exponential ladder (derived from the attempts, initial and max \
                         delay above). The same resilient retry runs during the initial load \
                         and the on-demand retry pass.",
                    );
                });
            });

        egui::CollapsingHeader::new("Tool paths")
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    egui::Grid::new("toolpaths").num_columns(2).show(ui, |ui| {
                        ui.label("yt-dlp path:");
                        tools_changed |= ui
                            .text_edit_singleline(&mut self.settings.ytdlp_path)
                            .lost_focus();
                        ui.end_row();
                        ui.label("ffmpeg path:");
                        tools_changed |= ui
                            .text_edit_singleline(&mut self.settings.ffmpeg_path)
                            .lost_focus();
                        ui.end_row();

                    });
                });
            });

        self.ui_authentication(ui, busy);

        if tools_changed {
            let _ = self.settings.save();
            self.recheck_tools();
        }
    }

    /// Settings → SoundCloud Authentication: pick a browser, see whether its
    /// cookies can actually be read.
    ///
    /// The probe is what makes this honest. Chromium on Windows seals its
    /// cookie database with app-bound encryption that yt-dlp cannot decrypt, so
    /// simply offering "Chrome" without checking would promise a sign-in that
    /// silently never happens.
    fn ui_authentication(&mut self, ui: &mut egui::Ui, busy: bool) {
        use crate::config::settings::CookieBrowser;

        let mut check_now = false;
        let mut changed = false;

        egui::CollapsingHeader::new("SoundCloud Authentication")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "Some tracks' audio is served only to a signed-in session. Sign in \
                         to SoundCloud in your browser, pick it here, then use Retry Failed \
                         Metadata — no cookie files to export.",
                    )
                    .size(12.0)
                    .weak(),
                );
                ui.add_space(4.0);

                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Browser:");
                        let mut selected = self.settings.cookie_browser;
                        egui::ComboBox::from_id_salt("cookie_browser")
                            .selected_text(selected.label())
                            .show_ui(ui, |ui| {
                                for option in CookieBrowser::ALL {
                                    if ui
                                        .selectable_value(&mut selected, option, option.label())
                                        .clicked()
                                    {
                                        changed = true;
                                    }
                                }
                            });
                        if selected != self.settings.cookie_browser {
                            self.settings.cookie_browser = selected;
                            changed = true;
                        }
                        if ui
                            .add_enabled(
                                !self.cookie_checking
                                    && self.settings.cookie_browser != CookieBrowser::None,
                                egui::Button::new("Check"),
                            )
                            .on_hover_text(
                                "Read the browser's cookie jar to confirm it can be used. \
                                 Runs locally — no network request.",
                            )
                            .clicked()
                        {
                            check_now = true;
                        }
                        if self.cookie_checking {
                            ui.spinner();
                        }
                    });

                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        let (text, color) = match (&self.cookie_status, self.cookie_checking) {
                            (_, true) => ("Checking...".to_string(), Color32::GRAY),
                            (None, _) if self.settings.cookie_browser == CookieBrowser::None => {
                                ("Not using cookies".to_string(), Color32::GRAY)
                            }
                            (None, _) => (
                                "Not checked yet — press Check".to_string(),
                                Color32::GRAY,
                            ),
                            (Some(s), _) => (
                                s.headline(),
                                if s.ok() {
                                    components::OK_GREEN
                                } else if matches!(
                                    s,
                                    crate::downloader::cookies::CookieStatus::Disabled
                                ) {
                                    Color32::GRAY
                                } else {
                                    components::ERR_RED
                                },
                            ),
                        };
                        ui.label(RichText::new(text).color(color).size(12.5));
                    });

                    if let Some(detail) =
                        self.cookie_status.as_ref().and_then(|s| s.detail())
                    {
                        ui.label(
                            RichText::new(detail).color(components::WARN_AMBER).size(12.0),
                        );
                    }

                    // The manual file is the escape hatch, not the workflow: it
                    // only appears once a browser has actually failed, so the
                    // normal user is never asked to export anything.
                    let failed = self
                        .cookie_status
                        .as_ref()
                        .is_some_and(|s| !s.ok() && *s != crate::downloader::cookies::CookieStatus::Disabled);
                    if failed || !self.settings.cookies_path.trim().is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label("Cookies file:")
                                .on_hover_text(
                                    "Fallback for browsers that cannot be read. Must be \
                                     Netscape format (what yt-dlp accepts) — a JSON export \
                                     will not work. Used in preference to the browser above.",
                                );
                            changed |= ui
                                .text_edit_singleline(&mut self.settings.cookies_path)
                                .lost_focus();
                            if ui.small_button("Browse...").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_title("Select a Netscape-format cookies.txt")
                                    .add_filter("Cookies", &["txt"])
                                    .pick_file()
                                {
                                    self.settings.cookies_path =
                                        path.to_string_lossy().into_owned();
                                    changed = true;
                                }
                            }
                            if !self.settings.cookies_path.trim().is_empty()
                                && ui.small_button("Clear").clicked()
                            {
                                self.settings.cookies_path.clear();
                                changed = true;
                            }
                        });
                        if self.settings.cookies_missing() {
                            ui.label(
                                RichText::new(
                                    "⚠ That file does not exist — it is being ignored.",
                                )
                                .color(components::WARN_AMBER)
                                .size(12.0),
                            );
                        } else if self.settings.cookies_wrong_format() {
                            ui.label(
                                RichText::new(
                                    "⚠ That file is not in Netscape format — it is being \
                                     ignored. Most browser \"export cookies\" extensions \
                                     save JSON, which yt-dlp cannot read; choose an export \
                                     option that says cookies.txt or Netscape.",
                                )
                                .color(components::WARN_AMBER)
                                .size(12.0),
                            );
                        }
                    }
                });
            });

        if changed {
            // A different browser invalidates the previous verdict.
            self.cookie_status = None;
            let _ = self.settings.save();
        }
        if check_now {
            self.start_cookie_check();
        }
    }

    /// Probe the selected browser's cookie jar in the background.
    fn start_cookie_check(&mut self) {
        use crate::config::settings::CookieBrowser;
        if self.settings.cookie_browser == CookieBrowser::None {
            self.cookie_status = Some(crate::downloader::cookies::CookieStatus::Disabled);
            return;
        }
        self.cookie_checking = true;
        self.cookie_status = None;
        let ytdlp = self.settings.ytdlp_path.clone();
        let browser = self.settings.cookie_browser;
        let log_file = self.settings.tool_log("yt-dlp.log");
        let tx = self.tx.clone();
        self.logs.push(format!(
            "Checking {} cookies (--cookies-from-browser {})...",
            browser.label(),
            browser.ytdlp_name().unwrap_or("none")
        ));
        self.rt.spawn(async move {
            let status = crate::downloader::cookies::probe(
                &ytdlp,
                browser,
                &CancellationToken::new(),
                log_file.as_deref(),
            )
            .await;
            tx.send(WorkerMsg::CookieStatus(status));
        });
    }

    fn ui_export_mode(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.label("Export Mode:");
                let mut mode = self.settings.export_mode;
                egui::ComboBox::from_id_salt("export_mode")
                    .selected_text(mode.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut mode,
                            ExportMode::Separate,
                            ExportMode::Separate.label(),
                        );
                        ui.selectable_value(
                            &mut mode,
                            ExportMode::Combined,
                            ExportMode::Combined.label(),
                        );
                    });
                if mode != self.settings.export_mode {
                    self.settings.export_mode = mode;
                    let _ = self.settings.save();
                }
            });

            // Combined-mode specific options.
            if self.settings.export_mode == ExportMode::Combined {
                ui.indent("combined_opts", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Playlist video name:");
                        let hint = self
                            .playlist_title
                            .clone()
                            .unwrap_or_else(|| "Playlist".into());
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.playlist_video_name)
                                .hint_text(hint)
                                .desired_width(220.0),
                        )
                        .on_hover_text("Leave empty to use the playlist's own name");
                        ui.label(RichText::new(".mp4").weak());
                    });

                    ui.horizontal(|ui| {
                        ui.label("Transition length:");
                        egui::ComboBox::from_id_salt("transition")
                            .selected_text(format!("{:.0} seconds", self.settings.transition_seconds))
                            .show_ui(ui, |ui| {
                                for secs in [0.0_f64, 1.0, 2.0, 3.0, 5.0] {
                                    let label = if secs == 0.0 {
                                        "None (hard cut)".to_string()
                                    } else {
                                        format!("{secs:.0} seconds")
                                    };
                                    ui.selectable_value(
                                        &mut self.settings.transition_seconds,
                                        secs,
                                        label,
                                    );
                                }
                            })
                            .response
                            .on_hover_text(
                                "Crossfade between consecutive tracks in the combined video",
                            );

                        ui.separator();
                        ui.checkbox(&mut self.settings.enable_chapters, "Enable chapters")
                            .on_hover_text("Embed one chapter marker per track");

                        ui.separator();
                        ui.checkbox(
                            &mut self.settings.continue_on_fail,
                            "Continue when a track fails",
                        )
                        .on_hover_text(
                            "If off, the whole export stops as soon as one track fails",
                        );
                    });
                });
            }
        });
    }

    fn ui_encoding(&mut self, ui: &mut egui::Ui, busy: bool) {
        let support = self.encoder_support.clone().unwrap_or_default();
        let detected = self.encoder_support.is_some();
        egui::CollapsingHeader::new("Encoding (hardware acceleration)")
            .default_open(false)
            .show(ui, |ui| {
                // ---- Encoder selection ------------------------------------
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Encoder:");
                        let mut changed = false;
                        egui::ComboBox::from_id_salt("encoder")
                            .selected_text(self.settings.encoder.label())
                            .show_ui(ui, |ui| {
                                for choice in EncoderChoice::ALL {
                                    let avail = support.choice_availability(choice);
                                    let enabled = avail.is_available();
                                    let selected = self.settings.encoder == choice;
                                    let resp = ui.add_enabled(
                                        enabled,
                                        egui::SelectableLabel::new(selected, choice.label()),
                                    );
                                    let resp = match avail.reason() {
                                        Some(_) if !detected => {
                                            resp.on_hover_text("Detecting encoders...")
                                        }
                                        Some(reason) => resp.on_hover_text(reason),
                                        None => resp,
                                    };
                                    if resp.clicked() && enabled {
                                        self.settings.encoder = choice;
                                        changed = true;
                                    }
                                }
                            });
                        if changed {
                            let _ = self.settings.save();
                        }
                    });

                    // What Auto/fallback actually resolves to right now.
                    let (resolved, note) = support
                        .resolve(self.settings.encoder, self.settings.hardware_decode);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Will use:").size(12.5).weak());
                        ui.label(
                            RichText::new(resolved.kind.full_label())
                                .size(12.5)
                                .color(components::OK_GREEN),
                        );
                        if !detected {
                            ui.label(RichText::new("(detecting...)").size(12.0).weak());
                        }
                    });
                    if let Some(note) = note {
                        ui.label(
                            RichText::new(note).size(12.0).color(components::WARN_AMBER),
                        );
                    }

                    // Hardware decode — experimental, off by default. GPU encode
                    // alone gives most of the win; decode can be flaky with the
                    // combine filter graph.
                    let gpu = resolved.kind != crate::video::encoder::EncoderKind::Cpu;
                    ui.add_enabled_ui(gpu, |ui| {
                        if ui
                            .checkbox(
                                &mut self.settings.hardware_decode,
                                "Hardware decode for combine (experimental)",
                            )
                            .on_hover_text(
                                "Offload decoding of the intermediate clips to the GPU during the \
                                 combine. Compositing (text, crossfades) always stays on the CPU. \
                                 Experimental: GPU encoding alone already gives most of the \
                                 speed-up, and hardware decode can be flaky with complex filter \
                                 graphs. Turn off if a combine pass fails.",
                            )
                            .changed()
                        {
                            let _ = self.settings.save();
                        }
                    });

                    // Chunk size — the memory lever for huge playlists. Offered
                    // as presets; 40 (the original limit) is the default so
                    // existing resume batches stay valid.
                    ui.horizontal(|ui| {
                        ui.label("Combine batch size:");
                        let mut chunk = self.settings.combine_chunk_size;
                        egui::ComboBox::from_id_salt("combine_chunk")
                            .selected_text(format!("{chunk} clips"))
                            .show_ui(ui, |ui| {
                                for preset in COMBINE_CHUNK_PRESETS {
                                    let label = if preset == 40 {
                                        format!("{preset} clips (default)")
                                    } else {
                                        format!("{preset} clips")
                                    };
                                    ui.selectable_value(&mut chunk, preset, label);
                                }
                            })
                            .response
                            .on_hover_text(
                                "Clips combined per ffmpeg pass. Fewer clips per pass use less RAM \
                                 (fewer simultaneous decoders) but need more passes. Lower this \
                                 only if very large playlists exhaust memory.",
                            );
                        if chunk != self.settings.combine_chunk_size {
                            self.settings.combine_chunk_size = chunk;
                            let _ = self.settings.save();
                        }
                    });

                    // Upper-level (batch-combine) size — the memory lever for the
                    // FINAL combine. Batch inputs are full videos, so only a few
                    // may be open at once; 4 is the default.
                    ui.horizontal(|ui| {
                        ui.label("Batch-combine size:");
                        let mut batch = self.settings.batch_combine_chunk_size;
                        egui::ComboBox::from_id_salt("batch_combine_chunk")
                            .selected_text(format!("{batch} batches"))
                            .show_ui(ui, |ui| {
                                for preset in BATCH_COMBINE_CHUNK_PRESETS {
                                    let label = if preset == 4 {
                                        format!("{preset} batches (default)")
                                    } else {
                                        format!("{preset} batches")
                                    };
                                    ui.selectable_value(&mut batch, preset, label);
                                }
                            })
                            .response
                            .on_hover_text(
                                "Intermediate batch files combined per upper-level ffmpeg pass. \
                                 Each is a full video, so fewer at once means much less RAM \
                                 during the final combine (at the cost of one more re-encode). \
                                 Keep this low on very large playlists.",
                            );
                        if batch != self.settings.batch_combine_chunk_size {
                            self.settings.batch_combine_chunk_size = batch;
                            let _ = self.settings.save();
                        }
                    });
                });

                if let Some(gpu) = support.gpus.first() {
                    ui.label(RichText::new(format!("GPU: {gpu}")).size(12.0).weak());
                }

                ui.separator();
                // ---- Benchmark --------------------------------------------
                ui.horizontal(|ui| {
                    let can_bench = !busy && !self.benchmarking;
                    if ui
                        .add_enabled(can_bench, egui::Button::new("⏱ Benchmark (30s sample)"))
                        .on_hover_text(
                            "Render a 30-second sample with the selected encoder and report \
                             time, FPS and size. Switch encoders and run again to compare.",
                        )
                        .clicked()
                    {
                        self.start_benchmark();
                    }
                    if self.benchmarking {
                        ui.spinner();
                        ui.label(RichText::new("Encoding sample...").size(12.5).weak());
                    }
                });
                if !self.benchmarks.is_empty() {
                    // Total video-seconds the loaded selection would produce, so
                    // each encoder's throughput can be projected to a full render.
                    let playlist_seconds: f64 = self
                        .tracks
                        .iter()
                        .filter(|t| t.selected)
                        .map(|t| t.play_seconds.max(1.0))
                        .sum();
                    let have_estimate = playlist_seconds > 0.0;
                    // The app renders at 30 fps, same as the benchmark sample.
                    let total_frames = playlist_seconds * 30.0;

                    egui::Grid::new("bench_results")
                        .num_columns(if have_estimate { 5 } else { 4 })
                        .spacing([14.0, 2.0])
                        .striped(true)
                        .show(ui, |ui| {
                            ui.label(RichText::new("Encoder").strong().size(12.0));
                            ui.label(RichText::new("Time").strong().size(12.0));
                            ui.label(RichText::new("Avg FPS").strong().size(12.0));
                            ui.label(RichText::new("Size").strong().size(12.0));
                            if have_estimate {
                                ui.label(RichText::new("Est. full render").strong().size(12.0));
                            }
                            ui.end_row();
                            for b in &self.benchmarks {
                                ui.label(RichText::new(&b.encoder).size(12.0));
                                ui.label(RichText::new(format!("{:.1}s", b.elapsed_s)).size(12.0));
                                ui.label(RichText::new(format!("{:.0}", b.fps)).size(12.0));
                                ui.label(
                                    RichText::new(format!(
                                        "{:.1} MB",
                                        b.size_bytes as f64 / 1_048_576.0
                                    ))
                                    .size(12.0),
                                );
                                if have_estimate {
                                    let secs = total_frames / b.fps.max(0.1);
                                    ui.label(
                                        RichText::new(crate::utils::filesystem::format_duration(
                                            secs,
                                        ))
                                        .size(12.0)
                                        .color(components::OK_GREEN),
                                    );
                                }
                                ui.end_row();
                            }
                        });
                    if have_estimate {
                        ui.label(
                            RichText::new(format!(
                                "Estimate for the {} selected track(s) (~{}) at each encoder's \
                                 measured rate.",
                                self.tracks.iter().filter(|t| t.selected).count(),
                                crate::utils::filesystem::format_duration(playlist_seconds),
                            ))
                            .size(11.0)
                            .weak(),
                        );
                    }
                }
            });
    }

    fn apply_global_max(&mut self) {
        let max = self.settings.max_track_seconds;
        if max > 0.0 {
            for t in &mut self.tracks {
                t.play_seconds = t.play_seconds.min(max);
            }
        } else {
            for t in &mut self.tracks {
                if t.duration > 0 {
                    t.play_seconds = t.duration as f64;
                }
            }
        }
    }

    fn ui_track_list(&mut self, ui: &mut egui::Ui) {
        if self.tracks.is_empty() {
            ui.centered_and_justified(|ui| {
                let text = if self.state == AppState::LoadingPlaylist {
                    "Loading playlist..."
                } else {
                    "Paste a SoundCloud playlist URL above and press Load Playlist"
                };
                ui.label(RichText::new(text).weak().size(15.0));
            });
            return;
        }

        let busy = self.busy();
        let retrying = self.retrying();
        let failed_meta = self.failed_meta_count();
        let permanent_meta = self.permanent_meta_count();
        let restricted_meta = self.restricted_meta_count();
        let meta_loading = self.meta_loading;
        let mut retry_clicked = false;
        let mut stop_retry_clicked = false;

        ui.horizontal(|ui| {
            if let Some(title) = &self.playlist_title {
                ui.label(
                    RichText::new(format!("{title}  ({} tracks)", self.tracks.len())).strong(),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.small_button("Select none").clicked() {
                        for t in &mut self.tracks {
                            t.selected = false;
                        }
                    }
                    if ui.small_button("Select all").clicked() {
                        for t in &mut self.tracks {
                            t.selected = true;
                        }
                    }
                });

                // "Retry Failed Metadata" appears once the initial metadata
                // pass has finished, and is disabled when nothing failed.
                if retrying {
                    ui.separator();
                    if ui.small_button("⏹ Stop retry").clicked() {
                        stop_retry_clicked = true;
                    }
                    ui.spinner();
                } else if !meta_loading {
                    ui.separator();
                    let button = egui::Button::new(
                        RichText::new(format!("⟳ Retry Failed Metadata ({failed_meta})"))
                            .size(12.5),
                    );
                    let resp = ui.add_enabled(!busy && failed_meta > 0, button);
                    if resp.clicked() {
                        retry_clicked = true;
                    }
                    let mut unavailable = String::new();
                    if permanent_meta > 0 {
                        unavailable.push_str(&format!(
                            "\n\n{permanent_meta} track(s) are permanently unavailable \
                             (deleted, private or region-locked) and are not retried — \
                             each row shows why."
                        ));
                    }
                    if restricted_meta > 0 {
                        unavailable.push_str(&format!(
                            "\n\n{restricted_meta} track(s) were refused to an anonymous \
                             client. Pick your browser under SoundCloud Authentication \
                             first, otherwise \
                             they will fail the same way."
                        ));
                    }
                    resp.on_hover_text(if failed_meta > 0 {
                        format!(
                            "Re-fetch metadata for the {failed_meta} track(s) that failed.\n\
                             Tracks that already loaded are left untouched.{unavailable}"
                        )
                    } else if permanent_meta > 0 {
                        format!("Nothing left to retry.{unavailable}")
                    } else {
                        "All tracks have metadata.".to_string()
                    });
                }
            });
        });
        ui.separator();

        if retry_clicked {
            self.start_retry_metadata(false);
        }
        if stop_retry_clicked {
            self.cancel_retry();
        }

        let global_max = self.settings.max_track_seconds;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for track in &mut self.tracks {
                    track_row(ui, track, global_max, busy);
                    ui.separator();
                }
            });
    }

    fn ui_bottom(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("⚠").color(components::ERR_RED).size(16.0));
                ui.label(RichText::new(&err).color(components::ERR_RED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("dismiss").clicked() {
                        self.error = None;
                    }
                });
            });
            ui.add_space(4.0);
        }

        self.ui_retry_status(ui);

        if let Some(p) = &self.progress {
            ui.add(
                egui::ProgressBar::new(p.frac)
                    .show_percentage()
                    .animate(self.state == AppState::Converting),
            );
            ui.label(RichText::new(&p.phase).size(13.0));
            if let Some(enc) = &self.active_encoder {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Encoding with:").size(12.5).weak());
                    ui.label(
                        RichText::new(enc).size(12.5).color(components::OK_GREEN),
                    );
                });
            }
            ui.add_space(4.0);
        }

        ui.horizontal(|ui| {
            let selected = self.tracks.iter().filter(|t| t.selected).count();
            let tools_ok = self.tools.ffmpeg.is_some() && self.tools.ytdlp.is_some();

            // Converting while a retry is in flight would put two sets of
            // yt-dlp requests on the wire — exactly what caused the failures.
            let start = ui.add_enabled(
                !self.busy() && !self.retrying() && selected > 0 && tools_ok,
                egui::Button::new(
                    RichText::new(format!("▶  Convert {selected} track(s)")).size(15.0),
                )
                .fill(components::ACCENT.gamma_multiply(0.85)),
            );
            if start.clicked() {
                self.on_convert_clicked();
            }
            if !tools_ok && self.tools.checked {
                ui.label(
                    RichText::new("install ffmpeg and yt-dlp to convert")
                        .color(components::ERR_RED)
                        .size(12.5),
                );
            }

            if self.busy() {
                if ui.button("⏹ Cancel").clicked() {
                    self.cancel_work();
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut self.show_logs, "Show logs");
            });
        });

        if self.show_logs {
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt("logs")
                .max_height(140.0)
                .stick_to_bottom(true)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for line in &self.logs {
                        ui.label(RichText::new(line).monospace().size(11.5).weak());
                    }
                });
        }
    }

    /// Live retry progress, or the summary of the last finished retry pass.
    fn ui_retry_status(&mut self, ui: &mut egui::Ui) {
        // A pass is running right now.
        if let Some(r) = &self.retry_progress {
            let (auto, total_pass, remaining_pass) = (r.auto, r.total, r.remaining);
            if auto {
                // Continuous recovery: report whole-playlist progress.
                let total = self.tracks.len();
                let resolved = self.resolved_meta_count();
                let frac = if total == 0 { 1.0 } else { resolved as f32 / total as f32 };
                ui.label(RichText::new("Metadata Recovery").strong().size(13.0));
                ui.add(egui::ProgressBar::new(frac).show_percentage().animate(true));
                ui.label(
                    RichText::new(format!("{resolved} / {total} recovered"))
                        .size(13.0)
                        .weak(),
                );
                ui.label(RichText::new("Recovering metadata...").size(12.5).weak());
            } else {
                let done = total_pass.saturating_sub(remaining_pass);
                let frac = if total_pass == 0 { 1.0 } else { done as f32 / total_pass as f32 };
                ui.add(egui::ProgressBar::new(frac).show_percentage().animate(true));
                ui.label(RichText::new("Retrying metadata...").size(13.0));
                ui.label(
                    RichText::new(format!("{remaining_pass} / {total_pass} remaining"))
                        .size(13.0)
                        .weak(),
                );
            }
            ui.add_space(4.0);
            return;
        }

        // Between passes: waiting out the back-off before the next automatic try.
        if let Some(at) = self.recovery_next_at {
            let total = self.tracks.len();
            let resolved = self.resolved_meta_count();
            let remaining = self.auto_recoverable_count();
            let frac = if total == 0 { 1.0 } else { resolved as f32 / total as f32 };
            let secs = at.saturating_duration_since(Instant::now()).as_secs();
            let mut stop = false;
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Metadata Recovery").strong().size(13.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⏹ Stop recovery").clicked() {
                            stop = true;
                        }
                    });
                });
                ui.add(egui::ProgressBar::new(frac).show_percentage());
                ui.label(
                    RichText::new(format!("{resolved} / {total} recovered"))
                        .size(13.0)
                        .weak(),
                );
                ui.label(
                    RichText::new(format!("{remaining} remaining — retrying in {secs}s..."))
                        .size(12.5)
                        .weak(),
                );
            });
            if stop {
                self.cancel_retry();
            }
            ui.add_space(4.0);
            return;
        }

        let Some(s) = &self.retry_summary else { return };
        let heading = if s.cancelled {
            "Metadata retry cancelled.".to_string()
        } else if s.gave_up {
            "Metadata retry stopped — SoundCloud is still rate limiting.".to_string()
        } else if s.auto {
            "Automatic metadata retry complete.".to_string()
        } else {
            "Metadata retry complete.".to_string()
        };
        let (recovered, still_failed, permanent, restricted) =
            (s.recovered, s.still_failed, s.permanent, s.restricted);
        // Only the tracks that could still succeed are worth pointing at the
        // button for; the other two categories get their own wording.
        let retryable = still_failed.saturating_sub(permanent + restricted);
        let hint = if retryable > 0 {
            Some("Use Retry Failed Metadata above to try the remaining tracks again.".to_string())
        } else if restricted > 0 {
            Some(format!(
                "{restricted} track(s) were refused to an anonymous client — they are not \
                 necessarily gone. Pick your browser under Settings → SoundCloud \
                 Authentication, then \
                 press Retry Failed Metadata."
            ))
        } else if permanent > 0 {
            Some(format!(
                "{permanent} track(s) are permanently unavailable and cannot be recovered — \
                 each row shows the reason. Retrying will not help."
            ))
        } else {
            None
        };
        let mut dismiss = false;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&heading).strong().size(13.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("dismiss").clicked() {
                        dismiss = true;
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Recovered:").size(12.5).weak());
                ui.label(
                    RichText::new(format!("{recovered} tracks"))
                        .color(components::OK_GREEN)
                        .size(12.5),
                );
                ui.separator();
                ui.label(RichText::new("Still failed:").size(12.5).weak());
                ui.label(
                    RichText::new(format!("{still_failed} tracks"))
                        .color(if still_failed > 0 {
                            components::ERR_RED
                        } else {
                            components::OK_GREEN
                        })
                        .size(12.5),
                );
                if restricted > 0 {
                    ui.separator();
                    ui.label(RichText::new("Restricted:").size(12.5).weak());
                    ui.label(
                        RichText::new(format!("{restricted} tracks"))
                            .color(components::WARN_AMBER)
                            .size(12.5),
                    )
                    .on_hover_text(
                        "SoundCloud refused these to an anonymous client. A cookies file \
                         may recover them — they are not confirmed unavailable.",
                    );
                }
                if permanent > 0 {
                    ui.separator();
                    ui.label(RichText::new("Unavailable:").size(12.5).weak());
                    ui.label(
                        RichText::new(format!("{permanent} tracks"))
                            .color(components::WARN_AMBER)
                            .size(12.5),
                    );
                }
            });
            if let Some(hint) = hint {
                ui.label(RichText::new(hint).size(12.0).weak());
            }
        });
        if dismiss {
            self.retry_summary = None;
        }
        ui.add_space(4.0);
    }

    /// The startup "Required tools missing" assistant. Renders a modal dialog
    /// and applies whatever the user chose afterwards.
    fn ui_setup_dialog(&mut self, ctx: &egui::Context) {
        if matches!(self.setup, SetupUi::Hidden) {
            return;
        }
        let ffmpeg_missing = self.tools.ffmpeg.is_none();
        let ytdlp_missing = self.tools.ytdlp.is_none();
        let mut action = SetupAction::None;

        // Dim the app behind the dialog.
        egui::Area::new("setup_dim".into())
            .fixed_pos(egui::Pos2::ZERO)
            .order(egui::Order::Background)
            .show(ctx, |ui| {
                let screen = ctx.screen_rect();
                ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(160));
            });

        egui::Window::new(RichText::new("Required tools missing").strong())
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(540.0);
                ui.spacing_mut().item_spacing.y = 8.0;

                match &self.setup {
                    SetupUi::Prompt | SetupUi::NoWinget => {
                        ui.label("SoundCloud -> MP4 requires these external tools:");
                        Self::missing_line(ui, "ffmpeg", ffmpeg_missing);
                        Self::missing_line(ui, "yt-dlp", ytdlp_missing);
                        ui.add_space(2.0);

                        if matches!(self.setup, SetupUi::Prompt) {
                            ui.label(
                                "Would you like SoundCloud -> MP4 to install them automatically \
                                 with winget?",
                            );
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .button(RichText::new("Install Automatically").strong())
                                    .clicked()
                                {
                                    action = SetupAction::Install;
                                }
                                if ui.button("Cancel").clicked() {
                                    action = SetupAction::Dismiss;
                                }
                            });
                        } else {
                            ui.colored_label(
                                components::ERR_RED,
                                "winget was not found, so automatic installation isn't available.",
                            );
                            ui.label(setup::manual_instructions());
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui.button("Close").clicked() {
                                    action = SetupAction::Dismiss;
                                }
                            });
                        }
                    }
                    SetupUi::Installing(log) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Installing required tools — please wait...");
                        });
                        ui.add_space(2.0);
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for line in log {
                                    ui.label(
                                        RichText::new(line).monospace().size(11.5).weak(),
                                    );
                                }
                            });
                    }
                    SetupUi::Failed { message, needs_elevation } => {
                        ui.colored_label(components::ERR_RED, "Installation failed.");
                        if *needs_elevation {
                            ui.label(
                                "Administrator permissions are required to install these tools.",
                            );
                        }
                        egui::ScrollArea::vertical()
                            .max_height(180.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.label(RichText::new(message).monospace().size(11.5));
                            });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if *needs_elevation
                                && ui
                                    .button(RichText::new("Restart as Administrator").strong())
                                    .clicked()
                            {
                                action = SetupAction::Elevate;
                            }
                            if ui.button("Try again").clicked() {
                                action = SetupAction::Install;
                            }
                            if ui.button("Cancel").clicked() {
                                action = SetupAction::Dismiss;
                            }
                        });
                    }
                    SetupUi::Success(message) => {
                        ui.colored_label(components::OK_GREEN, "✓ Tools ready");
                        ui.label(message);
                        ui.add_space(4.0);
                        if ui.button(RichText::new("Close").strong()).clicked() {
                            action = SetupAction::Close;
                        }
                    }
                    SetupUi::Hidden => {}
                }
            });

        match action {
            SetupAction::None => {}
            SetupAction::Install => self.start_install(),
            SetupAction::Dismiss => {
                self.setup_dismissed = true;
                self.setup = SetupUi::Hidden;
            }
            SetupAction::Elevate => setup::relaunch_as_admin(),
            SetupAction::Close => self.setup = SetupUi::Hidden,
        }
    }

    /// "A previous conversion was interrupted" — shown when a Combined export is
    /// started against a `.work` folder left behind by an earlier run. Resuming
    /// reuses every still-valid clip and batch; starting over deletes them.
    fn ui_resume_dialog(&mut self, ctx: &egui::Context) {
        if !self.resume_prompt {
            return;
        }
        #[derive(Clone, Copy)]
        enum Choice {
            None,
            Resume,
            StartOver,
            Cancel,
        }
        let mut choice = Choice::None;

        // Dim the app behind the dialog.
        egui::Area::new("resume_dim".into())
            .fixed_pos(egui::Pos2::ZERO)
            .order(egui::Order::Background)
            .show(ctx, |ui| {
                let screen = ctx.screen_rect();
                ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(160));
            });

        egui::Window::new(RichText::new("Resume previous conversion?").strong())
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.spacing_mut().item_spacing.y = 8.0;

                ui.label(RichText::new("A previous conversion was interrupted.").strong());
                ui.label(
                    "Rendered clips and combined batches from that run are still on disk. \
                     Resuming reuses everything that still checks out and only rebuilds what is \
                     missing or corrupt — the downloads and renders are not repeated.",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new("Resume previous conversion").strong())
                        .clicked()
                    {
                        choice = Choice::Resume;
                    }
                    if ui.button("Start over (delete work directory)").clicked() {
                        choice = Choice::StartOver;
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Choice::Cancel;
                    }
                });
                ui.colored_label(
                    components::OK_GREEN,
                    "Recommended: Resume.",
                );
                ui.label(
                    RichText::new(
                        "Starting over re-downloads and re-renders every track from scratch.",
                    )
                    .size(11.5)
                    .weak(),
                );
            });

        match choice {
            Choice::None => {}
            Choice::Resume => {
                self.resume_prompt = false;
                self.logs
                    .push("Resuming previous conversion — reusing valid clips and batches.".into());
                self.start_convert();
            }
            Choice::StartOver => {
                self.resume_prompt = false;
                let work_root = self.settings.output_folder.join(".work");
                match std::fs::remove_dir_all(&work_root) {
                    Ok(()) => self
                        .logs
                        .push(format!("Started over — deleted work directory: {}", work_root.display())),
                    Err(e) => self
                        .logs
                        .push(format!("Could not delete work directory ({e}); starting anyway.")),
                }
                self.start_convert();
            }
            Choice::Cancel => {
                self.resume_prompt = false;
            }
        }
    }

    fn missing_line(ui: &mut egui::Ui, name: &str, missing: bool) {
        if missing {
            ui.colored_label(components::ERR_RED, format!("✗ {name} — not found"));
        } else {
            ui.colored_label(components::OK_GREEN, format!("✓ {name} — installed"));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_messages();
        self.tick_recovery(ctx);

        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(10.0))
            .show(ctx, |ui| {
                self.ui_header(ui);
                ui.add_space(6.0);
                self.ui_input_row(ui);
                ui.add_space(6.0);
                self.ui_settings(ui);
                ui.add_space(2.0);
            });

        egui::TopBottomPanel::bottom("footer")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(10.0))
            .show(ctx, |ui| {
                self.ui_bottom(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.ui_track_list(ui);
        });

        self.ui_setup_dialog(ctx);
        self.ui_resume_dialog(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(token) = &self.cancel {
            token.cancel();
        }
        if let Some(token) = &self.meta_cancel {
            token.cancel();
        }
        if let Some(token) = &self.retry_cancel {
            token.cancel();
        }
        let _ = self.settings.save();
    }
}
