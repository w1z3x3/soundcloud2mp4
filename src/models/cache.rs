//! Persistent per-playlist metadata cache.
//!
//! Each playlist maps to one JSON file
//! `<config_dir>/soundcloud2mp4/metadata/<hash>.json`, where `<hash>` is a stable
//! hash of the normalised playlist URL. The file holds everything already
//! discovered about that playlist's tracks (title, uploader, duration, artwork,
//! chapter title, per-track metadata + render status).
//!
//! # Why it lives *outside* `.work`
//!
//! `.work` is the single *active conversion* workspace — its clips, batches and
//! `resume.json` — and it is deleted when a combined export finishes or the user
//! starts over. The metadata cache must outlive all of that: a resumed or simply
//! reloaded playlist should never re-hit SoundCloud for metadata it already has,
//! even across app restarts or after switching between several playlists. So the
//! caches are keyed per playlist and kept in a persistent app directory; `.work`
//! stays exactly as it was, the one active workspace.
//!
//! # Crash safety
//!
//! Every write serialises the whole file to a sibling `.tmp` and atomically
//! renames it over the target, so a crash mid-write leaves the previous good
//! file intact rather than a truncated one. Writes are incremental — the cache
//! is saved as soon as each track resolves, never only at the end.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::settings::Settings;
use crate::downloader::ytdlp::FlatEntry;
use crate::models::track::{MetaFailure, MetaState, Track, TrackMetadata};

/// Bump when the on-disk shape changes incompatibly; older files are then
/// ignored (treated as absent) and rebuilt.
const CACHE_VERSION: u32 = 1;

/// A serialisable projection of [`MetaState`] — the live state holds a
/// `MetaFailure` that is not itself persisted verbatim, only classified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CachedMeta {
    /// Fully resolved. Authoritative: never re-fetched, so chapter titles stay
    /// identical across resumes (see the module for `chapter_title`).
    Loaded,
    /// Never successfully fetched yet — a candidate for recovery.
    Missing,
    /// Fetched and failed. `permanent`/`restricted` mirror `FailureKind` so the
    /// recovery loop knows whether a retry could ever help.
    Failed {
        permanent: bool,
        restricted: bool,
        reason: Option<String>,
    },
}

/// A serialisable projection of the render outcome for the combined export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CachedRender {
    Pending,
    Done,
    Failed { reason: String },
}

/// Everything known about one track, as persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTrack {
    pub index: usize,
    pub id: String,
    pub url: String,
    pub title: String,
    pub uploader: String,
    pub duration: u64,
    pub thumbnail: Option<String>,
    /// Chapter title exactly as the combined export uses it ("uploader - title"),
    /// stored so a resumed export produces byte-identical chapter names.
    pub chapter_title: Option<String>,
    /// Clip file name once rendered, e.g. "clip_0007.mp4".
    pub clip_file: Option<String>,
    pub meta: CachedMeta,
    pub render: CachedRender,
}

impl CachedTrack {
    fn placeholder(index: usize, id: String, url: String, title: String) -> Self {
        Self {
            index,
            id,
            url,
            title,
            uploader: String::new(),
            duration: 0,
            thumbnail: None,
            chapter_title: None,
            clip_file: None,
            meta: CachedMeta::Missing,
            render: CachedRender::Pending,
        }
    }

    /// Fully resolved metadata that needs no re-fetch.
    pub fn is_complete(&self) -> bool {
        matches!(self.meta, CachedMeta::Loaded)
    }

    /// Whether the recovery loop should (automatically) try to fetch this track:
    /// it has never loaded and is not a dead end. Restricted and permanent
    /// failures are excluded — an identical anonymous request would fail the same
    /// way, so they are left to the manual button (restricted) or dropped
    /// entirely (permanent).
    pub fn needs_recovery(&self) -> bool {
        matches!(
            self.meta,
            CachedMeta::Missing | CachedMeta::Failed { permanent: false, restricted: false, .. }
        )
    }

    /// Rebuild the live GUI [`Track`] this cache entry represents.
    pub fn to_track(&self, global_max_seconds: f64) -> Track {
        let mut t = Track::placeholder(self.id.clone(), self.url.clone(), Some(self.title.clone()));
        match &self.meta {
            CachedMeta::Loaded => {
                let meta = TrackMetadata {
                    id: self.id.clone(),
                    title: self.title.clone(),
                    uploader: self.uploader.clone(),
                    duration: self.duration,
                    thumbnail: self.thumbnail.clone(),
                    url: self.url.clone(),
                };
                t.apply_metadata(&meta, global_max_seconds);
            }
            // Missing and retryable failures come back as "still loading" so the
            // recovery loop picks them up; only truly stuck rows are shown Failed.
            CachedMeta::Missing | CachedMeta::Failed { permanent: false, restricted: false, .. } => {
                t.meta = MetaState::Loading;
            }
            CachedMeta::Failed { permanent, restricted, reason } => {
                let failure = cached_failure(*permanent, *restricted, reason.clone());
                if failure.is_permanent() {
                    t.selected = false;
                }
                t.meta = MetaState::Failed(failure);
            }
        }
        t
    }
}

/// Reconstruct a display [`MetaFailure`] from the persisted classification.
fn cached_failure(permanent: bool, restricted: bool, reason: Option<String>) -> MetaFailure {
    let msg = reason
        .clone()
        .unwrap_or_else(|| "previously failed to load".to_string());
    if permanent {
        MetaFailure::permanent(msg.clone(), reason.unwrap_or(msg))
    } else if restricted {
        MetaFailure::restricted(msg.clone(), reason.unwrap_or(msg))
    } else {
        MetaFailure::transient(msg)
    }
}

/// One playlist's persisted metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistCache {
    pub version: u32,
    /// Normalised playlist URL this cache belongs to — the identity guard that
    /// keeps one playlist's cache from ever hydrating another.
    pub playlist_url: String,
    pub playlist_title: String,
    pub tracks: Vec<CachedTrack>,
}

impl PlaylistCache {
    /// Trim / lower-case / drop a trailing slash so trivially different spellings
    /// of the same playlist URL map to the same cache file.
    pub fn normalize_url(url: &str) -> String {
        url.trim().trim_end_matches('/').to_lowercase()
    }

    /// Stable 64-bit FNV-1a hash of the normalised URL, as hex — the cache file
    /// stem. Deliberately dependency-free and version-independent (unlike
    /// `DefaultHasher`, whose output is not guaranteed stable) so file names stay
    /// valid across builds.
    pub fn hash_url(url: &str) -> String {
        let norm = Self::normalize_url(url);
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in norm.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:016x}")
    }

    /// Where a given playlist URL's cache file lives.
    pub fn path_for(url: &str) -> PathBuf {
        Settings::metadata_cache_dir().join(format!("{}.json", Self::hash_url(url)))
    }

    /// Load the cache for `url`, or `None` when absent, unreadable, the wrong
    /// version, or (defensively) belongs to a different URL that happened to
    /// collide.
    pub fn load(url: &str) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path_for(url)).ok()?;
        let cache: Self = serde_json::from_str(&text).ok()?;
        if cache.version != CACHE_VERSION {
            return None;
        }
        if cache.playlist_url != Self::normalize_url(url) {
            return None;
        }
        Some(cache)
    }

    /// A fresh, empty cache for `url`.
    pub fn new(url: &str, title: &str) -> Self {
        Self {
            version: CACHE_VERSION,
            playlist_url: Self::normalize_url(url),
            playlist_title: title.to_string(),
            tracks: Vec::new(),
        }
    }

    /// Load the cache for `url` if present, else a fresh one.
    pub fn load_or_new(url: &str, title: &str) -> Self {
        Self::load(url).unwrap_or_else(|| Self::new(url, title))
    }

    /// Best-effort crash-safe write (temp file + atomic rename). A failed write
    /// never fails the caller — the cache is advisory and rebuilds itself.
    pub fn save(&self) {
        let path = Self::path_for(&self.playlist_url);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            // rename is atomic on the same volume and overwrites on Windows
            // (MoveFileEx MOVEFILE_REPLACE_EXISTING via std).
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Reconcile the cached track list with the playlist's current membership,
    /// carrying over everything already known for tracks still present (matched
    /// by SoundCloud id) and adding/dropping as the playlist changed. Keeps the
    /// cache correct when a track was added to or removed from the playlist
    /// between runs.
    pub fn sync_entries(&mut self, title: &str, entries: &[FlatEntry]) {
        self.playlist_title = title.to_string();
        let mut by_id: std::collections::HashMap<String, CachedTrack> =
            self.tracks.drain(..).map(|t| (t.id.clone(), t)).collect();
        self.tracks = entries
            .iter()
            .enumerate()
            .map(|(i, e)| match by_id.remove(&e.id) {
                Some(mut existing) => {
                    existing.index = i;
                    if !e.url.is_empty() {
                        existing.url = e.url.clone();
                    }
                    existing
                }
                None => CachedTrack::placeholder(
                    i,
                    e.id.clone(),
                    e.url.clone(),
                    e.title.clone().unwrap_or_else(|| format!("Track {}", e.id)),
                ),
            })
            .collect();
    }

    /// Number of tracks whose metadata is fully resolved.
    pub fn complete_count(&self) -> usize {
        self.tracks.iter().filter(|t| t.is_complete()).count()
    }

    /// Record freshly-resolved metadata for the track at `index`.
    pub fn record_meta(&mut self, index: usize, meta: &TrackMetadata) {
        if let Some(t) = self.tracks.get_mut(index) {
            t.id = meta.id.clone();
            t.title = meta.title.clone();
            t.uploader = meta.uploader.clone();
            t.duration = meta.duration;
            t.thumbnail = meta.thumbnail.clone();
            if !meta.url.is_empty() {
                t.url = meta.url.clone();
            }
            t.chapter_title = Some(format!("{} - {}", meta.uploader, meta.title));
            t.meta = CachedMeta::Loaded;
        }
    }

    /// Record a metadata failure for the track at `index`, classified so the
    /// recovery loop knows whether to try it again.
    pub fn record_meta_failure(&mut self, index: usize, failure: &MetaFailure) {
        if let Some(t) = self.tracks.get_mut(index) {
            // Never demote a track that already loaded (a later spurious failure
            // must not blow away good cached metadata / chapter titles).
            if t.is_complete() {
                return;
            }
            t.meta = CachedMeta::Failed {
                permanent: failure.is_permanent(),
                restricted: failure.is_restricted(),
                reason: failure.reason.clone(),
            };
        }
    }

    /// Record the render outcome (and clip file name) for the track at `index`.
    pub fn record_render(&mut self, index: usize, render: CachedRender, clip_file: Option<String>) {
        if let Some(t) = self.tracks.get_mut(index) {
            t.render = render;
            if clip_file.is_some() {
                t.clip_file = clip_file;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> FlatEntry {
        FlatEntry {
            id: id.into(),
            url: format!("https://soundcloud.com/a/{id}"),
            title: Some(format!("T{id}")),
        }
    }

    fn meta(id: &str) -> TrackMetadata {
        TrackMetadata {
            id: id.into(),
            title: format!("Song {id}"),
            uploader: "Artist".into(),
            duration: 100 + id.len() as u64,
            thumbnail: Some("https://a/x.jpg".into()),
            url: format!("https://soundcloud.com/a/{id}"),
        }
    }

    #[test]
    fn normalization_and_hash_are_stable() {
        assert_eq!(
            PlaylistCache::normalize_url("https://SoundCloud.com/a/Set/ "),
            "https://soundcloud.com/a/set"
        );
        // Trailing slash / case do not change the file identity.
        assert_eq!(
            PlaylistCache::hash_url("https://soundcloud.com/a/set"),
            PlaylistCache::hash_url("https://SoundCloud.com/a/set/")
        );
        // Different playlists get different files.
        assert_ne!(
            PlaylistCache::hash_url("https://soundcloud.com/a/one"),
            PlaylistCache::hash_url("https://soundcloud.com/a/two")
        );
    }

    #[test]
    fn records_and_classifies_metadata() {
        let mut c = PlaylistCache::new("https://soundcloud.com/a/set", "Set");
        c.sync_entries("Set", &[entry("1"), entry("2"), entry("3")]);
        assert_eq!(c.tracks.len(), 3);
        assert!(c.tracks.iter().all(|t| t.needs_recovery()));

        c.record_meta(0, &meta("1"));
        assert!(c.tracks[0].is_complete());
        assert!(!c.tracks[0].needs_recovery());
        assert_eq!(c.tracks[0].chapter_title.as_deref(), Some("Artist - Song 1"));
        assert_eq!(c.complete_count(), 1);

        c.record_meta_failure(1, &MetaFailure::permanent("gone", "deleted"));
        assert!(!c.tracks[1].needs_recovery(), "permanent is not auto-recovered");

        c.record_meta_failure(2, &MetaFailure::transient("rate limited"));
        assert!(c.tracks[2].needs_recovery(), "transient stays recoverable");
    }

    #[test]
    fn a_loaded_track_is_never_demoted_by_a_later_failure() {
        let mut c = PlaylistCache::new("https://soundcloud.com/a/set", "Set");
        c.sync_entries("Set", &[entry("1")]);
        c.record_meta(0, &meta("1"));
        c.record_meta_failure(0, &MetaFailure::transient("spurious"));
        assert!(c.tracks[0].is_complete(), "good metadata must survive a later failure");
    }

    #[test]
    fn sync_carries_over_known_tracks_when_membership_changes() {
        let mut c = PlaylistCache::new("https://soundcloud.com/a/set", "Set");
        c.sync_entries("Set", &[entry("1"), entry("2")]);
        c.record_meta(0, &meta("1"));
        // Track 2 removed, track 3 added; track 1 moves to index 1.
        c.sync_entries("Set", &[entry("9"), entry("1")]);
        assert_eq!(c.tracks.len(), 2);
        assert_eq!(c.tracks[1].id, "1");
        assert!(c.tracks[1].is_complete(), "known metadata carried over");
        assert_eq!(c.tracks[1].index, 1, "index updated to new position");
        assert!(c.tracks[0].needs_recovery(), "newly added track needs fetch");
    }

    /// The persistence contract: what is saved comes back byte-for-byte after a
    /// "restart" (a fresh `load`), and the URL identity guard keeps one
    /// playlist's file from loading as another's.
    #[test]
    fn saves_and_reloads_from_disk() {
        let url = format!("https://soundcloud.com/test/roundtrip-{}", std::process::id());
        let mut c = PlaylistCache::new(&url, "RT");
        c.sync_entries("RT", &[entry("1"), entry("2")]);
        c.record_meta(0, &meta("1"));
        c.record_render(0, CachedRender::Done, Some("clip_0000.mp4".into()));
        c.save();

        let loaded = PlaylistCache::load(&url).expect("cache should reload after a restart");
        assert_eq!(loaded.tracks.len(), 2);
        assert!(loaded.tracks[0].is_complete());
        assert_eq!(loaded.tracks[0].chapter_title.as_deref(), Some("Artist - Song 1"));
        assert_eq!(loaded.tracks[0].clip_file.as_deref(), Some("clip_0000.mp4"));
        assert_eq!(loaded.tracks[0].render, CachedRender::Done);
        assert!(loaded.tracks[1].needs_recovery());

        // A different playlist URL never resolves to this file.
        let other = format!("https://soundcloud.com/test/other-{}", std::process::id());
        assert!(PlaylistCache::load(&other).is_none());

        let _ = std::fs::remove_file(PlaylistCache::path_for(&url));
    }

    #[test]
    fn hydrated_track_reflects_cached_state() {
        let mut c = PlaylistCache::new("https://soundcloud.com/a/set", "Set");
        c.sync_entries("Set", &[entry("1"), entry("2"), entry("3")]);
        c.record_meta(0, &meta("1"));
        c.record_meta_failure(1, &MetaFailure::permanent("gone", "deleted"));
        // index 2 stays Missing.
        let loaded = c.tracks[0].to_track(0.0);
        assert_eq!(loaded.meta, MetaState::Loaded);
        assert_eq!(loaded.uploader, "Artist");
        let permanent = c.tracks[1].to_track(0.0);
        assert!(matches!(permanent.meta, MetaState::Failed(_)));
        assert!(!permanent.selected, "permanent failures are auto-deselected");
        let missing = c.tracks[2].to_track(0.0);
        assert_eq!(missing.meta, MetaState::Loading, "missing shows as still loading");
    }
}
