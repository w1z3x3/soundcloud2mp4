use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use crate::downloader::ytdlp::FailureKind;

/// Per-track processing state shown in the track list.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackStatus {
    Pending,
    Downloading,
    Rendering,
    Done(PathBuf),
    Failed(String),
}

/// Why a metadata fetch failed, and whether trying again could ever help.
///
/// The permanence verdict is decided once, where the yt-dlp error is still
/// available (`downloader::ytdlp::permanent_failure_reason`), and travels with
/// the failure from there. It used to be computed and then dropped at the
/// worker/GUI boundary, which left the GUI unable to tell a rate-limited track
/// from a deleted one: "Retry Failed Metadata" re-queued the dead tracks on
/// every click and they could never succeed.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaFailure {
    /// The full yt-dlp error, for the log and the hover tooltip.
    pub message: String,
    /// How this failure should be treated.
    pub kind: FailureKind,
    /// Short human reason for the row, when there is one to give.
    pub reason: Option<String>,
}

impl MetaFailure {
    /// A failure worth retrying (rate limiting, network trouble).
    pub fn transient(message: impl Into<String>) -> Self {
        Self { message: message.into(), kind: FailureKind::Retryable, reason: None }
    }

    /// SoundCloud refused an anonymous client. Repeating the identical request
    /// is pointless, but cookies or a newer yt-dlp may get through — so this is
    /// explicitly *not* a permanent verdict.
    pub fn restricted(message: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: FailureKind::Restricted,
            reason: Some(reason.into()),
        }
    }

    /// A failure that no number of retries can fix.
    pub fn permanent(message: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: FailureKind::Permanent,
            reason: Some(reason.into()),
        }
    }

    pub fn is_permanent(&self) -> bool {
        self.kind.is_permanent()
    }

    pub fn is_restricted(&self) -> bool {
        matches!(self.kind, FailureKind::Restricted)
    }

    /// Whether an automatic retry pass should pick this up. Restricted tracks
    /// are excluded: the identical anonymous request would fail identically.
    /// The manual button still offers them, since the user may have added a
    /// cookies file in between.
    pub fn is_auto_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    /// The one-line explanation shown in the track row.
    pub fn headline(&self) -> &str {
        self.reason
            .as_deref()
            .unwrap_or_else(|| self.message.lines().next().unwrap_or("unknown error"))
    }
}

/// State of the second-stage (per-track) metadata fetch.
#[derive(Debug, Clone, PartialEq)]
pub enum MetaState {
    /// Flat playlist row loaded; full metadata still being fetched.
    Loading,
    Loaded,
    /// Metadata could not be fetched/parsed; the failure names the missing
    /// fields or the yt-dlp error, and says whether a retry could ever help.
    /// Shown in the GUI instead of fake values.
    Failed(MetaFailure),
}

/// Fully resolved metadata for one track. Built via
/// `downloader::metadata::resolve_metadata` with SoundCloud fallback logic —
/// never populated with silent "Unknown" placeholders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackMetadata {
    pub id: String,
    pub title: String,
    pub uploader: String,
    /// Seconds.
    pub duration: u64,
    pub thumbnail: Option<String>,
    pub url: String,
}

/// A playlist entry as shown/edited in the GUI.
#[derive(Debug, Clone)]
pub struct Track {
    pub id: String,
    pub url: String,
    pub title: String,
    pub uploader: String,
    /// Seconds (0 until metadata is loaded).
    pub duration: u64,
    /// Remote thumbnail URL for the list view.
    pub thumbnail: Option<String>,
    pub selected: bool,
    /// User-editable: how many seconds of this song the video should play.
    pub play_seconds: f64,
    pub status: TrackStatus,
    pub meta: MetaState,
}

impl Track {
    /// Placeholder row created from a flat-playlist entry, before the
    /// per-track metadata fetch has completed.
    pub fn placeholder(id: String, url: String, title: Option<String>) -> Self {
        let title = title.unwrap_or_else(|| format!("Track {id}"));
        Self {
            id,
            url,
            title,
            uploader: String::new(),
            duration: 0,
            thumbnail: None,
            selected: true,
            play_seconds: 60.0,
            status: TrackStatus::Pending,
            meta: MetaState::Loading,
        }
    }

    /// Fill this row in from fully-resolved metadata.
    pub fn apply_metadata(&mut self, meta: &TrackMetadata, global_max_seconds: f64) {
        self.title = meta.title.clone();
        self.uploader = meta.uploader.clone();
        self.duration = meta.duration;
        self.thumbnail = meta.thumbnail.clone();
        if !meta.url.is_empty() {
            self.url = meta.url.clone();
        }
        let mut play = if meta.duration > 0 {
            meta.duration as f64
        } else {
            60.0
        };
        if global_max_seconds > 0.0 {
            play = play.min(global_max_seconds);
        }
        self.play_seconds = play;
        self.meta = MetaState::Loaded;
    }
}
