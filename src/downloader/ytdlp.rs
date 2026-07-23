use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use tokio_util::sync::CancellationToken;

use super::cookies::CookieSource;
use super::metadata::resolve_metadata;
use crate::config::settings::Settings;
use crate::models::messages::Tx;
use crate::models::track::TrackMetadata;
use crate::utils::process::{
    describe, run_capture, run_capture_status, run_streaming, tool_command,
};

/// One entry from `--flat-playlist`. For SoundCloud these carry ONLY id + url
/// (no title/uploader/duration/thumbnail), which is why a second per-track
/// metadata pass exists.
#[derive(Debug, Clone)]
pub struct FlatEntry {
    pub id: String,
    pub url: String,
    pub title: Option<String>,
}

pub fn looks_like_soundcloud_url(url: &str) -> bool {
    let u = url.trim().to_lowercase();
    (u.starts_with("http://") || u.starts_with("https://")) && u.contains("soundcloud.com")
}

fn save_debug_json(name: &str, content: &str, tx: &Tx) {
    let path = Settings::debug_dir().join(name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, content) {
        Ok(()) => tx.log(format!("Saved raw yt-dlp JSON to {}", path.display())),
        Err(e) => tx.log(format!("WARN: could not save {}: {e}", path.display())),
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    match &v[key] {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Parse the JSON produced by `yt-dlp --flat-playlist -J`.
/// Exposed (and fixture-tested) separately from the network call.
pub fn parse_flat_playlist(json: &str) -> Result<(String, Vec<FlatEntry>)> {
    let v: Value = serde_json::from_str(json).context("parsing yt-dlp playlist JSON")?;
    let title = str_field(&v, "title").unwrap_or_else(|| "Playlist".into());

    let mut entries = Vec::new();
    match v["entries"].as_array() {
        Some(list) => {
            for e in list {
                let Some(url) = str_field(e, "url").or_else(|| str_field(e, "webpage_url")) else {
                    continue;
                };
                let id = str_field(e, "id").unwrap_or_else(|| url.clone());
                entries.push(FlatEntry { id, url, title: str_field(e, "title") });
            }
        }
        None => {
            // A single-track URL yields one track object instead of entries.
            let url = str_field(&v, "webpage_url")
                .or_else(|| str_field(&v, "original_url"))
                .context("single track JSON has no webpage_url")?;
            let id = str_field(&v, "id").unwrap_or_else(|| url.clone());
            entries.push(FlatEntry { id, url, title: str_field(&v, "title") });
        }
    }

    if entries.is_empty() {
        bail!("no tracks found in this playlist");
    }
    Ok((title, entries))
}

/// Step 1: fetch the playlist track list quickly with `--flat-playlist -J`.
pub async fn fetch_playlist(
    ytdlp: &str,
    url: &str,
    cookies: &CookieSource,
    token: &CancellationToken,
    tx: &Tx,
    log_file: Option<&Path>,
) -> Result<(String, Vec<FlatEntry>)> {
    let mut cmd = tool_command(ytdlp);
    cmd.args(["--flat-playlist", "--no-warnings", "-J"]);
    cookies.apply(&mut cmd);
    cmd.arg(url.trim());
    tx.log(format!("Running: {}", describe(&cmd)));

    let (ok, stdout, stderr) = run_capture(cmd, ytdlp, token, log_file).await?;
    tx.log(format!(
        "yt-dlp exit={}, stdout={} bytes, stderr={} bytes",
        if ok { 0 } else { 1 },
        stdout.len(),
        stderr.len()
    ));
    save_debug_json("playlist_response.json", &stdout, tx);

    if !ok {
        let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
        bail!(
            "yt-dlp could not read this playlist:\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    parse_flat_playlist(&stdout)
}

/// A failed metadata fetch, with enough detail for the retry pass to decide
/// whether backing off is worthwhile and for the log to name the exit code.
#[derive(Debug, Clone)]
pub struct MetaError {
    /// What the GUI shows in the row (same wording as before).
    pub message: String,
    /// yt-dlp's exit code, when the process actually ran to completion.
    pub exit_code: Option<i32>,
    /// How this failure should be treated — see [`classify_failure`].
    pub kind: FailureKind,
    /// Short human reason for the row, when there is one to give.
    pub reason: Option<&'static str>,
    /// The fetch was aborted by cancellation, not by a failure.
    pub cancelled: bool,
}

impl MetaError {
    /// How the exit code reads in a log line.
    pub fn code_str(&self) -> String {
        self.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "n/a".into())
    }

    /// Worth retrying after a back-off: neither cancelled nor already known to
    /// be pointless to repeat.
    pub fn transient(&self) -> bool {
        !self.cancelled && self.kind.is_retryable()
    }

    /// The track is gone for good.
    pub fn permanent_reason(&self) -> Option<&'static str> {
        self.kind.is_permanent().then_some(self.reason).flatten()
    }

    /// What the retry engine should do about this failure.
    pub fn decision(&self) -> RetryDecision {
        RetryDecision::from_failure_kind(self.kind)
    }
}

/// Tracks that are genuinely gone, each paired with the reason shown in the
/// track row. These are excluded from every retry pass rather than being
/// offered again forever. First match wins, so more specific needles first.
///
/// Two messages are deliberately *absent*:
///
/// - `403 Forbidden` — reads like a permissions error, but SoundCloud returns
///   403 when it is throttling. Listing it here would classify throttled
///   failures on a large playlist as permanent and disable the back-off
///   entirely.
/// - `This video is DRM protected` — see [`RESTRICTED_FAILURES`]; that one is
///   not a DRM determination at all.
const PERMANENT_FAILURES: [(&str, &str); 10] = [
    ("has been removed", "Track was removed by the uploader"),
    ("was deleted", "Track was deleted"),
    ("private", "Track is private"),
    ("unavailable in your country", "Track is not available in your country"),
    ("unsupported url", "Not a supported SoundCloud page"),
    ("is not a valid url", "Not a supported SoundCloud page"),
    ("404", "Track no longer exists"),
    ("not found", "Track no longer exists"),
    ("does not exist", "Track no longer exists"),
    ("410", "Track no longer exists"),
];

/// SoundCloud refused this track to an anonymous client. The track itself is
/// fine; retrying the *same* anonymous request will not help, but signing in
/// (browser cookies) or a newer extractor may.
///
/// # Why `DRM protected` is in this list and not the permanent one
///
/// It reads like a verdict but is really yt-dlp's fallback message. Inspecting
/// affected tracks with `-F` shows the pattern:
///
/// ```text
/// [soundcloud] 1234567890: Downloading info JSON            <- succeeds
/// WARNING: [soundcloud] 1234567890: hls_mp3 format not found
/// WARNING: [soundcloud] 1234567890: http_mp3 format not found
/// ERROR:   [soundcloud] 1234567890: This video is DRM protected
/// ```
///
/// yt-dlp emits it only after the plain-MP3 formats fail to resolve, leaving
/// nothing but encrypted HLS. Querying `api-v2.soundcloud.com` for those same
/// tracks shows they are entirely healthy — `policy: ALLOW`, `streamable:
/// true`, `snipped: false`, full duration and title present — and that they
/// *do* advertise an unencrypted `mp3_1_0` progressive transcoding. Resolving
/// that transcoding anonymously is what returns 404.
///
/// So the audio is not encrypted; SoundCloud is withholding the plain stream
/// from anonymous clients. Classifying this as permanent was wrong: it hid a
/// recoverable track behind a verdict the evidence did not support.
/// First match wins, so more specific needles first. Everything here maps to the
/// [`RetryDecision::RefreshCookiesAndRetry`] action: the request may get through
/// once with fresh browser cookies, so it is retried a single time after a cookie
/// refresh before being reported (see [`crate::pipeline`]).
const RESTRICTED_FAILURES: [(&str, &str); 8] = [
    ("drm protected", RESTRICTED_REASON),
    ("drm-protected", RESTRICTED_REASON),
    // Authentication / sign-in required — a fresh signed-in cookie jar may work.
    ("requires authentication", "SoundCloud requires a sign-in for this track"),
    ("authentication required", "SoundCloud requires a sign-in for this track"),
    ("login required", "SoundCloud requires a sign-in for this track"),
    ("http error 401", "SoundCloud requires a sign-in for this track"),
    // Stale / expired cookies — refreshing the browser jar is exactly the fix.
    ("cookies are no longer valid", EXPIRED_COOKIES_REASON),
    ("cookies have expired", EXPIRED_COOKIES_REASON),
];

/// Deliberately describes what was observed rather than asserting a cause.
pub const RESTRICTED_REASON: &str =
    "SoundCloud restricted access (authentication or extractor issue)";

/// Shown when the failure looks like the signed-in session has lapsed.
pub const EXPIRED_COOKIES_REASON: &str =
    "SoundCloud authentication may have expired — refreshing browser cookies";

/// Shown when yt-dlp exits cleanly but hands back no track object at all
/// (literally `null` on stdout). SoundCloud does this for entries that are
/// still listed in a playlist but no longer resolvable.
pub const NO_DATA_REASON: &str = "SoundCloud returned no data for this track";

/// How a failed fetch should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Rate limiting or a transient network/server error. Retry after a
    /// back-off; this is the default for anything unrecognised.
    Retryable,
    /// SoundCloud refused an anonymous client. Repeating the identical request
    /// is pointless, but browser cookies (Settings → SoundCloud Authentication) or a newer
    /// yt-dlp may get through, so it is *not* a permanent verdict.
    Restricted,
    /// The track is genuinely gone; nothing will recover it.
    Permanent,
}

impl FailureKind {
    /// Worth automatically re-requesting exactly as before.
    pub fn is_retryable(self) -> bool {
        matches!(self, FailureKind::Retryable)
    }
    pub fn is_permanent(self) -> bool {
        matches!(self, FailureKind::Permanent)
    }
}

/// What a caller should *do* about a metadata failure — the action view of
/// [`FailureKind`]. Derived centrally from [`classify_failure`] so the retry
/// engine (and anything else) never re-matches error strings itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Transient — rate limiting, 5xx, timeouts, connection resets, TLS/DNS
    /// trouble, proxy failures. Pause on the shared cooldown and retry.
    Retry,
    /// Access looks like it needs (fresh) authentication — DRM/restricted,
    /// 401, login-required, or expired cookies. Refresh the browser cookies and
    /// retry once, then fall back to normal handling.
    RefreshCookiesAndRetry,
    /// The track is genuinely gone (deleted / private / 404 / unsupported URL)
    /// or the request is malformed. Never retry.
    PermanentFailure,
}

impl RetryDecision {
    pub fn from_failure_kind(kind: FailureKind) -> Self {
        match kind {
            FailureKind::Retryable => RetryDecision::Retry,
            FailureKind::Restricted => RetryDecision::RefreshCookiesAndRetry,
            FailureKind::Permanent => RetryDecision::PermanentFailure,
        }
    }
}

/// The single action classifier: what to do about a yt-dlp error, from its
/// stderr. A thin action-oriented wrapper over [`classify_failure`] so callers
/// share one source of truth and no string matching is scattered around.
pub fn retry_decision(stderr: &str) -> RetryDecision {
    RetryDecision::from_failure_kind(classify_failure(stderr).0)
}

/// Classify a yt-dlp failure from its stderr.
///
/// Restricted is checked before permanent so that `DRM protected` is not
/// swallowed by a looser needle, and unrecognised errors stay retryable — the
/// safe default, since misjudging a rate limit loses a track entirely while
/// misjudging a dead one costs a few delayed requests.
///
/// Transient signatures (HTTP 429 / "Too Many Requests" / "rate limit" / 5xx /
/// "connection reset"/"aborted" / "broken pipe" / timeouts / "network
/// unreachable" / TLS handshake / DNS lookup / proxy errors) are intentionally
/// *not* enumerated: they are exactly the unrecognised-error default, so they
/// all resolve to [`FailureKind::Retryable`]. The tests pin this down.
pub fn classify_failure(stderr: &str) -> (FailureKind, Option<&'static str>) {
    let s = stderr.to_lowercase();
    if let Some((_, reason)) = RESTRICTED_FAILURES.iter().find(|(n, _)| s.contains(n)) {
        return (FailureKind::Restricted, Some(reason));
    }
    if let Some((_, reason)) = PERMANENT_FAILURES.iter().find(|(n, _)| s.contains(n)) {
        return (FailureKind::Permanent, Some(reason));
    }
    (FailureKind::Retryable, None)
}

/// Add the cookies hint to a *download* failure that looks like withheld
/// access, so the conversion error says something actionable instead of just
/// echoing yt-dlp.
///
/// Metadata for these tracks now resolves (see `--ignore-no-formats-error` in
/// [`fetch_track_info_detailed`]), so they appear as ordinary rows and are
/// selected for export like any other — but the audio stream is still refused
/// to an anonymous client, and the failure surfaces here instead.
pub fn annotate_download_failure(message: &str) -> String {
    match classify_failure(message) {
        (FailureKind::Restricted, _) => format!(
            "{message}\n\n{RESTRICTED_REASON}. SoundCloud is withholding this track's \
             audio from an anonymous client — the track itself may be fine. Setting a \
             browser under Settings → SoundCloud Authentication may allow it."
        ),
        _ => message.to_string(),
    }
}

/// The reason this track can never load, or `None` if it may yet be recovered.
pub fn permanent_failure_reason(stderr: &str) -> Option<&'static str> {
    match classify_failure(stderr) {
        (FailureKind::Permanent, reason) => reason,
        _ => None,
    }
}

/// Is this yt-dlp failure worth retrying after a back-off?
///
/// Both the polarity and the exclusions here were derived from observed
/// behaviour on large playlists, not from guesswork. SoundCloud's rate
/// limiting never announces itself as HTTP 429; it shows up as either
///
/// ```text
/// ERROR: [soundcloud:user] x: Unable to download JSON metadata:
///        HTTP Error 403: Forbidden
/// ERROR: 'NoneType' object is not subscriptable      (empty API body)
/// ```
///
/// Two earlier approaches proved wrong against real throttling: an allow-list
/// of "429 / rate limit / timeout" needles matched almost none of the observed
/// failures, and treating 403 as permanent matched none of them. Both silently
/// disabled the back-off ladder.
///
/// Hence: everything is retryable unless it clearly says the track is gone.
/// Misjudging a dead track costs three delayed requests; misjudging a rate
/// limit loses the track entirely.
pub fn is_transient_failure(stderr: &str) -> bool {
    permanent_failure_reason(stderr).is_none()
}

/// Step 2: fetch full metadata for one track with `--dump-single-json --no-download`.
pub async fn fetch_track_info(
    ytdlp: &str,
    entry: &FlatEntry,
    cookies: &CookieSource,
    token: &CancellationToken,
    tx: &Tx,
    log_file: Option<&Path>,
) -> Result<TrackMetadata, String> {
    fetch_track_info_detailed(ytdlp, entry, cookies, token, tx, log_file)
        .await
        .map_err(|e| e.message)
}

/// The metadata fetch used by both the initial load and the retry pass.
/// Identical yt-dlp invocation either way — retries differ only in pacing.
pub async fn fetch_track_info_detailed(
    ytdlp: &str,
    entry: &FlatEntry,
    cookies: &CookieSource,
    token: &CancellationToken,
    tx: &Tx,
    log_file: Option<&Path>,
) -> Result<TrackMetadata, MetaError> {
    let mut cmd = tool_command(ytdlp);
    cmd.args([
        "--dump-single-json",
        "--no-download",
        "--no-playlist",
        "--no-warnings",
        // This stage wants title/uploader/duration/artwork and nothing else,
        // but yt-dlp still resolves playable formats and fails the whole
        // extraction when it cannot — even though the track's info JSON came
        // back fine. That is what makes "DRM protected" tracks appear to fail
        // metadata even though their metadata is available the entire time.
        // With this flag they return complete metadata.
        "--ignore-no-formats-error",
    ]);
    cookies.apply(&mut cmd);
    cmd.arg(&entry.url);
    tx.log(format!("Running: {}", describe(&cmd)));

    let (status, stdout, stderr) = match run_capture_status(cmd, ytdlp, token, log_file).await {
        Ok(v) => v,
        Err(e) => {
            let message = format!("{e:#}");
            let cancelled = token.is_cancelled() || message.contains("cancelled");
            return Err(MetaError {
                message,
                exit_code: None,
                kind: FailureKind::Retryable,
                reason: None,
                cancelled,
            });
        }
    };

    save_debug_json(&format!("track_{}.json", entry.id), &stdout, tx);

    if !status.success() {
        let tail: Vec<&str> = stderr.lines().rev().take(4).collect();
        let (kind, reason) = classify_failure(&stderr);
        return Err(MetaError {
            message: format!(
                "yt-dlp failed (metadata fetch):\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            ),
            exit_code: status.code(),
            kind,
            reason,
            cancelled: false,
        });
    }

    let v: Value = serde_json::from_str(&stdout).map_err(|e| MetaError {
        message: format!("invalid yt-dlp JSON: {e}"),
        exit_code: status.code(),
        // Truncated JSON is usually a killed/short-circuited transfer.
        kind: FailureKind::Retryable,
        reason: None,
        cancelled: false,
    })?;
    let meta = resolve_metadata(&v).map_err(|message| MetaError {
        message,
        exit_code: status.code(),
        // yt-dlp succeeded and the fields are genuinely absent — retrying the
        // exact same request will produce the exact same JSON.
        kind: FailureKind::Permanent,
        reason: Some(NO_DATA_REASON),
        cancelled: false,
    })?;
    tx.log(format!(
        "yt-dlp returned: title: {} | uploader: {} | duration: {}s | thumbnail: {}",
        meta.title,
        meta.uploader,
        meta.duration,
        meta.thumbnail.as_deref().unwrap_or("(none)")
    ));
    Ok(meta)
}

/// Download one track's audio + thumbnail + info.json into `dir`.
/// Files are named `track.<ext>` so they are easy to locate afterwards.
pub async fn download_track(
    ytdlp: &str,
    url: &str,
    dir: &Path,
    quality_kbps: u32,
    cookies: &CookieSource,
    token: &CancellationToken,
    tx: &Tx,
    log_file: Option<&Path>,
) -> Result<()> {
    let mut cmd = tool_command(ytdlp);
    cmd.args([
        "--extract-audio",
        "--audio-format",
        "mp3",
        "--audio-quality",
        &format!("{quality_kbps}K"),
        "--write-thumbnail",
        "--write-info-json",
        "--no-playlist",
        "--no-part",
        "--newline",
        "--no-warnings",
    ]);
    cookies.apply(&mut cmd);
    cmd.arg("-o")
        .arg(dir.join("track.%(ext)s"))
        .arg(url);

    run_streaming(cmd, ytdlp, token, tx, "yt-dlp", log_file).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAT_FIXTURE: &str =
        include_str!("../../tests/fixtures/soundcloud_flat_playlist.json");

    #[test]
    fn parses_real_flat_playlist_fixture() {
        let (title, entries) = parse_flat_playlist(FLAT_FIXTURE).unwrap();
        assert_eq!(title, "example-playlist");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "1000000001");
        assert_eq!(entries[0].url, "https://soundcloud.com/example_user/track-one");
        // SoundCloud flat entries genuinely have no title — this documents
        // why the second metadata pass is required.
        assert_eq!(entries[0].title, None);
    }

    #[test]
    fn rejects_non_soundcloud_urls() {
        assert!(!looks_like_soundcloud_url("not a url"));
        assert!(!looks_like_soundcloud_url("https://example.com/sets/x"));
        assert!(looks_like_soundcloud_url(
            "https://soundcloud.com/example_user/sets/example-playlist"
        ));
    }

    #[test]
    fn rate_limit_and_server_errors_are_transient() {
        for stderr in [
            // The two signatures SoundCloud rate limiting actually produces.
            // Both are seen on large playlist loads; neither mentions 429 and
            // one of them looks exactly like a permissions error.
            "ERROR: [soundcloud:user] p: Unable to download JSON metadata: \
             HTTP Error 403: Forbidden (caused by <HTTPError 403: Forbidden>)",
            "ERROR: 'NoneType' object is not subscriptable",
            "ERROR: unable to download API page: HTTP Error 429: Too Many Requests",
            "ERROR: Unable to download webpage: HTTP Error 503: Service Unavailable",
            "ERROR: unable to download: The read operation timed out",
            "ERROR: [soundcloud] Unable to connect: Connection reset by peer",
            // Unrecognised errors get the benefit of the doubt.
            "ERROR: something nobody has seen before",
        ] {
            assert!(is_transient_failure(stderr), "should be transient: {stderr}");
        }
    }

    #[test]
    fn missing_or_private_tracks_are_not_transient() {
        for stderr in [
            "ERROR: [soundcloud] 123: Track is private and cannot be accessed",
            "ERROR: [soundcloud] Unable to extract track: track not found",
            "ERROR: Unsupported URL: https://soundcloud.com/x",
            "ERROR: [soundcloud] 9: HTTP Error 404: Not Found",
            "ERROR: This track has been removed by the uploader",
        ] {
            assert!(!is_transient_failure(stderr), "should be permanent: {stderr}");
        }
    }

    #[test]
    fn empty_playlist_is_an_error() {
        let err = parse_flat_playlist(r#"{"title":"x","entries":[]}"#).unwrap_err();
        assert!(err.to_string().contains("no tracks"));
    }

    #[test]
    fn retry_decision_maps_every_failure_kind() {
        assert_eq!(RetryDecision::from_failure_kind(FailureKind::Retryable), RetryDecision::Retry);
        assert_eq!(
            RetryDecision::from_failure_kind(FailureKind::Restricted),
            RetryDecision::RefreshCookiesAndRetry
        );
        assert_eq!(
            RetryDecision::from_failure_kind(FailureKind::Permanent),
            RetryDecision::PermanentFailure
        );
    }

    #[test]
    fn transient_signatures_all_decide_retry() {
        // Every category the spec lists as retryable must resolve to Retry —
        // they are all the unrecognised-error default.
        for stderr in [
            "ERROR: HTTP Error 429: Too Many Requests",
            "ERROR: Too Many Requests",
            "ERROR: You are being rate limited, please slow down",
            "ERROR: The service is temporarily unavailable",
            "ERROR: Connection reset by peer",
            "ERROR: Connection aborted",
            "ERROR: [Errno 32] Broken pipe",
            "ERROR: The read operation timed out",
            "ERROR: Network is unreachable",
            "ERROR: TLS handshake failed",
            "ERROR: Unable to download: [SSL] handshake failure",
            "ERROR: Temporary failure in name resolution",
            "ERROR: getaddrinfo failed: DNS lookup failed",
            "ERROR: HTTP Error 500: Internal Server Error",
            "ERROR: HTTP Error 502: Bad Gateway",
            "ERROR: HTTP Error 503: Service Unavailable",
            "ERROR: HTTP Error 504: Gateway Timeout",
            "ERROR: unable to connect to proxy",
            // The two SoundCloud throttle signatures (403 / NoneType).
            "ERROR: HTTP Error 403: Forbidden",
            "ERROR: 'NoneType' object is not subscriptable",
        ] {
            assert_eq!(retry_decision(stderr), RetryDecision::Retry, "should retry: {stderr}");
        }
    }

    #[test]
    fn auth_and_cookie_signatures_decide_cookie_refresh() {
        for stderr in [
            "ERROR: [soundcloud] 1: This video is DRM protected",
            "ERROR: [soundcloud] 1: This video is DRM-protected",
            "ERROR: This track requires authentication",
            "ERROR: Authentication required to access this track",
            "ERROR: Login required to view this track",
            "ERROR: HTTP Error 401: Unauthorized",
            "ERROR: The provided cookies are no longer valid",
            "ERROR: Your cookies have expired, please log in again",
        ] {
            assert_eq!(
                retry_decision(stderr),
                RetryDecision::RefreshCookiesAndRetry,
                "should refresh cookies: {stderr}"
            );
        }
    }

    #[test]
    fn dead_track_signatures_decide_permanent() {
        for stderr in [
            "ERROR: [soundcloud] 9: HTTP Error 404: Not Found",
            "ERROR: HTTP Error 410: Gone",
            "ERROR: This track is private",
            "ERROR: This track has been removed by the uploader",
            "ERROR: This track was deleted",
            "ERROR: This track is unavailable in your country",
            "ERROR: Unsupported URL: https://soundcloud.com/x",
            "ERROR: track not found",
        ] {
            assert_eq!(
                retry_decision(stderr),
                RetryDecision::PermanentFailure,
                "should be permanent: {stderr}"
            );
        }
    }
}
