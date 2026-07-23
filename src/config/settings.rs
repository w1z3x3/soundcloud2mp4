use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

pub const QUALITIES: [&str; 5] = ["64", "128", "192", "256", "320"];
pub const RESOLUTIONS: [&str; 4] = ["1280x720", "1920x1080", "2560x1440", "3840x2160"];

/// How the selected tracks are exported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportMode {
    /// One MP4 per track (original behavior).
    Separate,
    /// A single MP4 containing every selected track in playlist order.
    Combined,
}

impl Default for ExportMode {
    fn default() -> Self {
        ExportMode::Separate
    }
}

impl ExportMode {
    pub fn label(&self) -> &'static str {
        match self {
            ExportMode::Separate => "Separate videos",
            ExportMode::Combined => "One long playlist video",
        }
    }
}

/// Which browser yt-dlp should lift SoundCloud cookies from.
///
/// Only the three the GUI offers; yt-dlp supports more, but every extra entry
/// is another support question and these cover almost everyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CookieBrowser {
    #[default]
    None,
    Chrome,
    Edge,
    Firefox,
}

impl CookieBrowser {
    pub const ALL: [CookieBrowser; 4] = [
        CookieBrowser::None,
        CookieBrowser::Chrome,
        CookieBrowser::Edge,
        CookieBrowser::Firefox,
    ];

    /// The name `--cookies-from-browser` expects, or `None` for logged out.
    pub fn ytdlp_name(self) -> Option<&'static str> {
        match self {
            CookieBrowser::None => None,
            CookieBrowser::Chrome => Some("chrome"),
            CookieBrowser::Edge => Some("edge"),
            CookieBrowser::Firefox => Some("firefox"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CookieBrowser::None => "None",
            CookieBrowser::Chrome => "Chrome",
            CookieBrowser::Edge => "Edge",
            CookieBrowser::Firefox => "Firefox",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Audio bitrate in kbps, e.g. "320".
    pub default_quality: String,
    /// Video resolution "WIDTHxHEIGHT".
    pub default_resolution: String,
    pub output_folder: PathBuf,
    /// Global maximum seconds a song may play in its video. 0 = unlimited.
    pub max_track_seconds: f64,
    /// Slow Ken-Burns style zoom on the cover.
    pub effect_zoom: bool,
    /// Fade in/out for both video and audio.
    pub effect_fade: bool,
    /// Executable names or full paths; overridable if not on PATH.
    pub ytdlp_path: String,
    pub ffmpeg_path: String,
    /// Browser to read SoundCloud cookies from — the normal way to sign in.
    ///
    /// SoundCloud serves some tracks' plain MP3 streams only to a logged-in
    /// session: the transcodings are advertised, but resolving them anonymously
    /// returns 404 and yt-dlp then reports the track as DRM protected. Pointing
    /// yt-dlp at a browser the user is already signed into avoids any manual
    /// export step. `None` = run anonymously, as before.
    pub cookie_browser: CookieBrowser,
    /// Fallback Netscape-format cookies file (`--cookies`), for when a browser
    /// cannot be read — notably Chromium on Windows, whose app-bound encryption
    /// yt-dlp cannot decrypt. Hidden in the GUI until a browser probe fails, so
    /// no one is asked to produce one in the normal case. Takes precedence when
    /// set, since it is only ever set deliberately.
    pub cookies_path: String,
    /// When enabled, write logs/app.log, logs/yt-dlp.log and logs/ffmpeg.log.
    pub debug_mode: bool,

    /// Which H.264 encoder to use. `Auto` picks the best available hardware
    /// encoder (NVENC → QSV → AMF) and falls back to CPU `libx264`.
    pub encoder: crate::video::encoder::EncoderChoice,
    /// Request `-hwaccel` decoding for the combine stage when the selected
    /// hardware encoder supports it. **Experimental and off by default** —
    /// hardware decode can be flaky with complex filter graphs, and GPU
    /// *encoding* alone already gives most of the speed-up. When on, decoding is
    /// offloaded to the GPU while the filter graph (scale / xfade / text) stays
    /// on the CPU.
    pub hardware_decode: bool,
    /// Maximum clips fed to a single FIRST-LEVEL combine pass. Smaller batches
    /// cap the RAM one ffmpeg process needs (fewer simultaneous decoders + a
    /// smaller filter graph), which is what keeps 500-track playlists from
    /// exhausting memory.
    pub combine_chunk_size: usize,
    /// Maximum intermediate BATCH files fed to a single UPPER-LEVEL combine pass
    /// (rounds 2+). Kept far smaller than `combine_chunk_size` on purpose: a
    /// batch input is a full re-encoded video, vastly heavier to decode than a
    /// still-image clip, so only a few may be open at once without exhausting
    /// memory. This is the knob that bounds peak memory during the final combine
    /// on very large playlists — see the two-tier ladder in
    /// [`crate::video::concat`].
    pub batch_combine_chunk_size: usize,

    /// Separate per-track videos, or one combined playlist video.
    pub export_mode: ExportMode,
    /// Output file name (without extension) for the combined video.
    /// Empty -> falls back to the playlist title.
    pub playlist_video_name: String,
    /// Crossfade length between tracks in the combined video (seconds).
    pub transition_seconds: f64,
    /// Embed chapter markers (one per track) in the combined video.
    pub enable_chapters: bool,
    /// When a track fails, continue the export instead of aborting it.
    pub continue_on_fail: bool,

    /// Automatically run background metadata recovery: after a playlist loads
    /// (or an interrupted conversion is resumed), keep re-fetching any track
    /// whose metadata is still missing — backing off when rate limited and
    /// continuing until every track resolves — so the user never has to press
    /// "Retry Failed Metadata" repeatedly. When off, only the manual button
    /// runs. Default on.
    pub auto_recover_metadata: bool,

    /// Retry transient metadata failures (HTTP 429, 5xx, network errors) inline
    /// during the initial load and the retry pass, pausing on a shared cooldown
    /// instead of marking the track failed on the first error. Default on — this
    /// is what stops a brief SoundCloud rate limit from skipping dozens of
    /// tracks. When off, each track is fetched exactly once.
    pub auto_metadata_retry: bool,
    /// Maximum fetch attempts per track (including the first). The shared
    /// exponential back-off has one rung per attempt. Clamped to 1..=10.
    pub metadata_retry_max_attempts: u32,
    /// First rung of the exponential rate-limit back-off, in seconds. The ladder
    /// is `initial, 2×, 4×, …` capped at `retry_max_delay_secs`.
    pub retry_initial_delay_secs: u64,
    /// Longest rung of the exponential rate-limit back-off, in seconds. The
    /// doubling never exceeds this.
    pub retry_max_delay_secs: u64,
    /// On an access-restricted failure (DRM/auth/expired cookies), refresh the
    /// selected browser's cookies and retry the request once before reporting
    /// it. Default on. No effect when no browser is selected.
    pub auto_cookie_refresh: bool,

    /// Pause before each request of the "Retry Failed Metadata" pass, in
    /// milliseconds. Deliberately gentler than the initial load (which has no
    /// delay at all) because failures there are usually rate limiting.
    pub retry_delay_ms: u64,
    /// Parallel workers for the retry pass. Clamped to 1..=4 and never above
    /// the initial load's concurrency, so a retry can't hammer harder than the
    /// load that already failed.
    pub retry_concurrency: usize,
}

impl Default for Settings {
    fn default() -> Self {
        let output_folder = dirs::video_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("SoundCloud Videos");
        Self {
            default_quality: "320".into(),
            default_resolution: "1920x1080".into(),
            output_folder,
            max_track_seconds: 0.0,
            effect_zoom: false,
            effect_fade: true,
            ytdlp_path: "yt-dlp".into(),
            ffmpeg_path: "ffmpeg".into(),
            cookie_browser: CookieBrowser::None,
            cookies_path: String::new(),
            debug_mode: false,
            encoder: crate::video::encoder::EncoderChoice::Auto,
            hardware_decode: false,
            combine_chunk_size: DEFAULT_COMBINE_CHUNK_SIZE,
            batch_combine_chunk_size: DEFAULT_BATCH_COMBINE_CHUNK_SIZE,
            export_mode: ExportMode::Separate,
            playlist_video_name: String::new(),
            transition_seconds: 2.0,
            enable_chapters: true,
            continue_on_fail: true,
            auto_recover_metadata: true,
            auto_metadata_retry: true,
            metadata_retry_max_attempts: DEFAULT_METADATA_RETRY_MAX_ATTEMPTS,
            retry_initial_delay_secs: DEFAULT_RETRY_INITIAL_DELAY_SECS,
            retry_max_delay_secs: DEFAULT_RETRY_MAX_DELAY_SECS,
            auto_cookie_refresh: true,
            retry_delay_ms: 500,
            retry_concurrency: 2,
        }
    }
}

/// Default attempts per track: 6 rungs of exponential back-off (5→10→20→40→60→60)
/// span ~3 minutes, enough to ride out a typical SoundCloud rate limit without
/// hanging on a hard one (the abort guard and background recovery take over).
pub const DEFAULT_METADATA_RETRY_MAX_ATTEMPTS: u32 = 6;
pub const METADATA_RETRY_ATTEMPTS_RANGE: std::ops::RangeInclusive<u32> = 1..=10;
/// Exponential back-off endpoints: start at 5s, double, cap at 60s.
pub const DEFAULT_RETRY_INITIAL_DELAY_SECS: u64 = 5;
pub const DEFAULT_RETRY_MAX_DELAY_SECS: u64 = 60;
pub const RETRY_INITIAL_DELAY_RANGE: std::ops::RangeInclusive<u64> = 1..=120;
pub const RETRY_MAX_DELAY_RANGE: std::ops::RangeInclusive<u64> = 5..=600;

/// Bounds for the retry pacing knobs, enforced on read so a hand-edited
/// config.json can't turn the retry into a more aggressive load.
pub const RETRY_DELAY_MS_RANGE: std::ops::RangeInclusive<u64> = 250..=5_000;
pub const RETRY_CONCURRENCY_RANGE: std::ops::RangeInclusive<usize> = 1..=4;

/// Build an exponential back-off ladder: `initial, 2×, 4×, …` capped at `max`,
/// with one rung per attempt. Pure and unit-tested. A zero rung count yields an
/// empty ladder (the shared gate then falls back to a small default).
pub fn exponential_backoff(initial: Duration, max: Duration, rungs: usize) -> Vec<Duration> {
    let floor = Duration::from_secs(1);
    let mut step = initial.max(floor);
    let ceil = max.max(step);
    let mut out = Vec::with_capacity(rungs);
    for _ in 0..rungs {
        let capped = step.min(ceil);
        out.push(capped);
        step = (capped * 2).min(ceil);
    }
    out
}

/// Default clips-per-first-level-pass. Kept at 40 (the original combine limit)
/// so existing resume batches stay valid and the leaf level stays shallow; lower
/// it only if memory pressure is observed while combining clips.
pub const DEFAULT_COMBINE_CHUNK_SIZE: usize = 40;
/// Batch-size choices offered in the UI. Smaller = less RAM per ffmpeg process
/// but more passes; larger = fewer passes but a bigger filter graph in flight.
pub const COMBINE_CHUNK_PRESETS: [usize; 5] = [16, 24, 32, 40, 64];
/// Allowed range for the chunk size. The upper bound is the combine stage's
/// command-line-safe maximum; the lower bound is the minimum a crossfade pass
/// can join.
pub const COMBINE_CHUNK_SIZE_RANGE: std::ops::RangeInclusive<usize> =
    2..=crate::video::concat::MAX_COMBINE_CHUNK;

/// Default intermediate-batches-per-upper-level-pass. Deliberately small: a
/// batch input is a full re-encoded video, so opening only 4 at once keeps peak
/// memory low even on 1000+ track playlists, at the cost of one extra lossy
/// intermediate generation on the combined portion. See [`crate::video::concat`].
pub const DEFAULT_BATCH_COMBINE_CHUNK_SIZE: usize = 4;
/// Upper-level batch-size choices offered in the UI.
pub const BATCH_COMBINE_CHUNK_PRESETS: [usize; 4] = [2, 4, 8, 16];
/// Allowed range for the upper-level batch size. Two is the crossfade minimum;
/// the upper bound matches the command-line-safe maximum, though small values
/// are the point.
pub const BATCH_COMBINE_CHUNK_SIZE_RANGE: std::ops::RangeInclusive<usize> =
    2..=crate::video::concat::MAX_COMBINE_CHUNK;

/// Does this look like the Netscape cookie file yt-dlp expects?
///
/// The format is a header comment plus tab-separated 7-field lines. A JSON
/// export — what most "export cookies" extensions produce, and the reason this
/// check exists — starts with `[` or `{` and has no tabs at all.
pub fn looks_like_netscape_cookies(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        return false;
    }
    if trimmed
        .lines()
        .next()
        .is_some_and(|l| l.to_lowercase().contains("netscape http cookie file"))
    {
        return true;
    }
    // Otherwise accept it only if some data line really has 7 tab-separated
    // fields, which is what yt-dlp parses.
    trimmed
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .any(|l| l.split('\t').count() == 7)
}

impl Settings {
    pub fn resolution(&self) -> (u32, u32) {
        let mut it = self.default_resolution.split('x');
        let w = it.next().and_then(|s| s.parse().ok()).unwrap_or(1920);
        let h = it.next().and_then(|s| s.parse().ok()).unwrap_or(1080);
        (w, h)
    }

    pub fn bitrate_k(&self) -> u32 {
        self.default_quality.parse().unwrap_or(320)
    }

    /// Clips per combine pass, clamped to the supported range so a hand-edited
    /// config can't drive a pass over the Windows command-line limit or below a
    /// crossfade's two-input minimum.
    pub fn combine_chunk(&self) -> usize {
        self.combine_chunk_size.clamp(
            *COMBINE_CHUNK_SIZE_RANGE.start(),
            *COMBINE_CHUNK_SIZE_RANGE.end(),
        )
    }

    /// Intermediate batches per upper-level combine pass, clamped to the
    /// supported range so a hand-edited config can't drive an unbounded number of
    /// heavy batch decoders into one ffmpeg process.
    pub fn batch_combine_chunk(&self) -> usize {
        self.batch_combine_chunk_size.clamp(
            *BATCH_COMBINE_CHUNK_SIZE_RANGE.start(),
            *BATCH_COMBINE_CHUNK_SIZE_RANGE.end(),
        )
    }

    /// Retry delay, clamped to the supported range.
    pub fn retry_delay(&self) -> Duration {
        Duration::from_millis(
            self.retry_delay_ms
                .clamp(*RETRY_DELAY_MS_RANGE.start(), *RETRY_DELAY_MS_RANGE.end()),
        )
    }

    /// Maximum fetch attempts per track, clamped to the supported range.
    pub fn metadata_retry_attempts(&self) -> u32 {
        self.metadata_retry_max_attempts.clamp(
            *METADATA_RETRY_ATTEMPTS_RANGE.start(),
            *METADATA_RETRY_ATTEMPTS_RANGE.end(),
        )
    }

    /// First rung of the exponential back-off, clamped to the supported range.
    pub fn retry_initial_delay(&self) -> Duration {
        Duration::from_secs(self.retry_initial_delay_secs.clamp(
            *RETRY_INITIAL_DELAY_RANGE.start(),
            *RETRY_INITIAL_DELAY_RANGE.end(),
        ))
    }

    /// Longest rung of the exponential back-off, clamped to the supported range
    /// and never below the initial delay.
    pub fn retry_max_delay(&self) -> Duration {
        let max = Duration::from_secs(self.retry_max_delay_secs.clamp(
            *RETRY_MAX_DELAY_RANGE.start(),
            *RETRY_MAX_DELAY_RANGE.end(),
        ));
        max.max(self.retry_initial_delay())
    }

    /// The shared rate-limit back-off ladder: exponential from the initial delay,
    /// doubling to the maximum, one rung per configured attempt.
    pub fn retry_backoff(&self) -> Vec<Duration> {
        exponential_backoff(
            self.retry_initial_delay(),
            self.retry_max_delay(),
            self.metadata_retry_attempts() as usize,
        )
    }

    /// Retry worker count, clamped to the supported range.
    pub fn retry_workers(&self) -> usize {
        self.retry_concurrency.clamp(
            *RETRY_CONCURRENCY_RANGE.start(),
            *RETRY_CONCURRENCY_RANGE.end(),
        )
    }

    fn app_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("soundcloud2mp4")
    }

    /// Raw yt-dlp JSON responses are saved here (always, they are tiny).
    pub fn debug_dir() -> PathBuf {
        Self::app_dir().join("debug")
    }

    /// Persistent per-playlist metadata caches live here, one JSON file per
    /// playlist (keyed by a hash of its URL). Deliberately *not* inside `.work`:
    /// `.work` is wiped when a conversion finishes or the user starts over, but
    /// these caches must survive that, app restarts, and switching playlists —
    /// see [`crate::models::cache`].
    pub fn metadata_cache_dir() -> PathBuf {
        Self::app_dir().join("metadata")
    }

    /// app.log / yt-dlp.log / ffmpeg.log live here when debug_mode is on.
    pub fn logs_dir() -> PathBuf {
        Self::app_dir().join("logs")
    }

    /// Where yt-dlp should get cookies for every request this app makes.
    ///
    /// The browser dropdown is the primary control and wins whenever it is set.
    /// The file is only consulted when no browser is chosen — a leftover path
    /// from an earlier attempt must never silently override a deliberate
    /// browser selection.
    pub fn cookie_source(&self) -> crate::downloader::cookies::CookieSource {
        use crate::downloader::cookies::CookieSource;
        if self.cookie_browser != CookieBrowser::None {
            return CookieSource::Browser(self.cookie_browser);
        }
        match self.cookies_file() {
            Some(path) => CookieSource::File(path),
            None => CookieSource::None,
        }
    }

    /// The configured cookies file, if it exists *and* is actually the format
    /// yt-dlp accepts.
    ///
    /// Both checks matter. yt-dlp aborts outright on a missing or unparseable
    /// `--cookies` path, so a stale or wrong-format setting would turn into a
    /// total failure to load anything — and browser "export cookies" add-ons
    /// very often produce JSON, which yt-dlp rejects. Better to ignore the file
    /// and say why than to break every request with it.
    pub fn cookies_file(&self) -> Option<PathBuf> {
        let trimmed = self.cookies_path.trim();
        if trimmed.is_empty() {
            return None;
        }
        let path = PathBuf::from(trimmed);
        if !path.is_file() {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        looks_like_netscape_cookies(&text).then_some(path)
    }

    /// A cookies file is configured but the path does not exist.
    pub fn cookies_missing(&self) -> bool {
        let trimmed = self.cookies_path.trim();
        !trimmed.is_empty() && !PathBuf::from(trimmed).is_file()
    }

    /// A cookies file exists but is not Netscape format — the single most
    /// common mistake, since most browser extensions export JSON.
    pub fn cookies_wrong_format(&self) -> bool {
        let trimmed = self.cookies_path.trim();
        if trimmed.is_empty() {
            return false;
        }
        let path = PathBuf::from(trimmed);
        match std::fs::read_to_string(&path) {
            Ok(text) => !looks_like_netscape_cookies(&text),
            Err(_) => false,
        }
    }

    /// Path of a tool log file, if debug mode is enabled.
    pub fn tool_log(&self, name: &str) -> Option<PathBuf> {
        if self.debug_mode {
            Some(Self::logs_dir().join(name))
        } else {
            None
        }
    }

    pub fn config_path() -> PathBuf {
        Self::app_dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("invalid config {}: {e}; using defaults", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(v: &[Duration]) -> Vec<u64> {
        v.iter().map(|d| d.as_secs()).collect()
    }

    #[test]
    fn exponential_backoff_doubles_and_caps() {
        let ladder = exponential_backoff(Duration::from_secs(5), Duration::from_secs(60), 6);
        assert_eq!(secs(&ladder), vec![5, 10, 20, 40, 60, 60]);
    }

    #[test]
    fn exponential_backoff_respects_a_small_cap() {
        let ladder = exponential_backoff(Duration::from_secs(10), Duration::from_secs(10), 4);
        assert_eq!(secs(&ladder), vec![10, 10, 10, 10]);
    }

    #[test]
    fn exponential_backoff_max_below_initial_is_lifted_to_initial() {
        // A nonsensical max < initial must not produce a shrinking ladder.
        let ladder = exponential_backoff(Duration::from_secs(30), Duration::from_secs(5), 3);
        assert_eq!(secs(&ladder), vec![30, 30, 30]);
    }

    #[test]
    fn exponential_backoff_edge_counts() {
        assert!(exponential_backoff(Duration::from_secs(5), Duration::from_secs(60), 0).is_empty());
        assert_eq!(
            secs(&exponential_backoff(Duration::from_secs(5), Duration::from_secs(60), 1)),
            vec![5]
        );
    }

    #[test]
    fn default_settings_produce_the_documented_ladder() {
        let s = Settings::default();
        assert_eq!(secs(&s.retry_backoff()), vec![5, 10, 20, 40, 60, 60]);
        assert_eq!(s.metadata_retry_attempts(), 6);
        assert!(s.auto_metadata_retry);
        assert!(s.auto_cookie_refresh);
    }

    #[test]
    fn retry_knobs_are_clamped_on_read() {
        let mut s = Settings::default();
        s.metadata_retry_max_attempts = 999;
        s.retry_initial_delay_secs = 0;
        s.retry_max_delay_secs = 100_000;
        assert_eq!(s.metadata_retry_attempts(), 10);
        assert!(s.retry_initial_delay() >= Duration::from_secs(1));
        assert!(s.retry_max_delay() <= Duration::from_secs(600));
    }
}
