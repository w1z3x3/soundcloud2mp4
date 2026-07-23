use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::models::track::TrackMetadata;
use crate::utils::filesystem::find_file_with_ext;

/// Everything the renderer needs about a downloaded track.
#[derive(Debug)]
pub struct DownloadedTrack {
    pub meta: TrackMetadata,
    pub audio: PathBuf,
    /// Local cover image, if yt-dlp saved one.
    pub cover: Option<PathBuf>,
}

fn value_as_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// First non-empty string among the given top-level keys.
fn first_string(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| value_as_string(&v[*k]))
}

/// Resolve yt-dlp JSON (playlist entry or `--dump-single-json` output) into
/// complete track metadata using SoundCloud-aware fallbacks:
///
/// - title:     `title` → `track` → `id`
/// - uploader:  `uploader` → `artist` → `creator` → `channel` → `uploader_id`
/// - duration:  `duration` → `duration_ms / 1000` → 0
/// - thumbnail: `thumbnail` → last entry of `thumbnails[]`
/// - url:       `webpage_url` → `original_url` → `url`
///
/// Fields that cannot be resolved at all produce an error naming exactly what
/// is missing — values are never silently replaced with "Unknown".
pub fn resolve_metadata(v: &Value) -> Result<TrackMetadata, String> {
    let id = first_string(v, &["id", "display_id"]);

    let title = first_string(v, &["title", "track"]).or_else(|| id.clone());
    let uploader = first_string(
        v,
        &["uploader", "artist", "creator", "channel", "uploader_id"],
    );

    let duration = v["duration"]
        .as_f64()
        .or_else(|| v["duration_ms"].as_f64().map(|ms| ms / 1000.0))
        .unwrap_or(0.0);

    let thumbnail = value_as_string(&v["thumbnail"]).or_else(|| {
        v["thumbnails"]
            .as_array()
            .and_then(|arr| arr.iter().rev().find_map(|t| value_as_string(&t["url"])))
    });

    let url = first_string(v, &["webpage_url", "original_url", "url"]);

    let mut missing = Vec::new();
    if id.is_none() {
        missing.push("id");
    }
    if title.is_none() {
        missing.push("title/track");
    }
    if uploader.is_none() {
        missing.push("uploader/artist/creator/channel");
    }
    if url.is_none() {
        missing.push("webpage_url/original_url/url");
    }
    if !missing.is_empty() {
        return Err(format!(
            "yt-dlp JSON is missing required field(s): {}",
            missing.join(", ")
        ));
    }

    Ok(TrackMetadata {
        id: id.unwrap(),
        title: title.unwrap(),
        uploader: uploader.unwrap(),
        duration: duration.round().max(0.0) as u64,
        thumbnail,
        url: url.unwrap(),
    })
}

/// Validate that a finished download left everything we need in `dir`,
/// then resolve its metadata. Fails with a "Download incomplete: ..." message
/// naming exactly what is missing.
pub fn read_downloaded(dir: &Path) -> Result<DownloadedTrack> {
    let info_path = dir.join("track.info.json");
    if !info_path.exists() {
        bail!("Download incomplete: missing metadata file track.info.json");
    }
    let text = std::fs::read_to_string(&info_path)
        .with_context(|| format!("reading {}", info_path.display()))?;
    let value: Value = serde_json::from_str(&text).context("parsing track.info.json")?;
    let meta = resolve_metadata(&value)
        .map_err(|e| anyhow::anyhow!("parsing track.info.json: {e}"))?;

    let audio = find_file_with_ext(dir, &["mp3", "m4a", "opus", "ogg", "wav"]);
    let Some(audio) = audio else {
        bail!("Download incomplete: missing audio file (no mp3/m4a in work dir)");
    };

    let cover = find_file_with_ext(dir, &["jpg", "jpeg", "png", "webp"]);
    // If the track's metadata says a thumbnail exists but yt-dlp did not save
    // one, the download is incomplete; only tracks that genuinely have no
    // artwork proceed without a cover (a placeholder is generated later).
    if cover.is_none() && meta.thumbnail.is_some() {
        bail!("Download incomplete: missing thumbnail (metadata lists one, no image file was saved)");
    }

    Ok(DownloadedTrack { meta, audio, cover })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TRACK_FIXTURE: &str = include_str!("../../tests/fixtures/soundcloud_track.json");

    #[test]
    fn resolves_real_soundcloud_track_fixture() {
        let v: Value = serde_json::from_str(TRACK_FIXTURE).unwrap();
        let meta = resolve_metadata(&v).expect("fixture should resolve");
        assert_eq!(meta.title, "track-one");
        assert_eq!(meta.uploader, "example_user");
        assert_eq!(meta.duration, 30);
        assert_eq!(meta.url, "https://soundcloud.com/example_user/track-one");
        let thumb = meta.thumbnail.expect("fixture has thumbnails");
        assert!(thumb.starts_with("https://"), "bad thumbnail: {thumb}");
    }

    #[test]
    fn title_falls_back_to_track_then_id() {
        let v = json!({"id": "123", "track": "From Track Field",
                       "uploader": "u", "webpage_url": "https://x"});
        assert_eq!(resolve_metadata(&v).unwrap().title, "From Track Field");

        let v = json!({"id": "123", "uploader": "u", "webpage_url": "https://x"});
        assert_eq!(resolve_metadata(&v).unwrap().title, "123");
    }

    #[test]
    fn uploader_fallback_chain() {
        for key in ["uploader", "artist", "creator", "channel", "uploader_id"] {
            let v = json!({"id": "1", "title": "t", key: "someone", "url": "https://x"});
            assert_eq!(resolve_metadata(&v).unwrap().uploader, "someone", "key={key}");
        }
    }

    #[test]
    fn duration_falls_back_to_duration_ms_then_zero() {
        let base = json!({"id": "1", "title": "t", "uploader": "u", "url": "https://x"});
        let mut with_ms = base.clone();
        with_ms["duration_ms"] = json!(215500.0);
        assert_eq!(resolve_metadata(&with_ms).unwrap().duration, 216);
        assert_eq!(resolve_metadata(&base).unwrap().duration, 0);
    }

    #[test]
    fn thumbnail_falls_back_to_thumbnails_array_and_may_be_absent() {
        let v = json!({"id": "1", "title": "t", "uploader": "u", "url": "https://x",
            "thumbnails": [{"url": "https://a/small.jpg"}, {"url": "https://a/original.jpg"}]});
        assert_eq!(
            resolve_metadata(&v).unwrap().thumbnail.as_deref(),
            Some("https://a/original.jpg")
        );
        let v = json!({"id": "1", "title": "t", "uploader": "u", "url": "https://x"});
        assert_eq!(resolve_metadata(&v).unwrap().thumbnail, None);
    }

    #[test]
    fn missing_fields_are_reported_not_defaulted() {
        let v = json!({"duration": 10});
        let err = resolve_metadata(&v).unwrap_err();
        assert!(err.contains("id"), "{err}");
        assert!(err.contains("title"), "{err}");
        assert!(err.contains("uploader"), "{err}");
        assert!(!err.contains("Unknown"), "{err}");
    }

    #[test]
    fn numeric_id_is_accepted() {
        let v = json!({"id": 1000000001u64, "title": "t", "uploader": "u", "url": "https://x"});
        assert_eq!(resolve_metadata(&v).unwrap().id, "1000000001");
    }

    fn touch(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sc2mp4_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_thumbnail_file_fails_when_metadata_lists_one() {
        let dir = temp_dir("thumb_missing");
        touch(&dir, "track.info.json",
            r#"{"id":"1","title":"t","uploader":"u","url":"https://x","thumbnail":"https://a/t.jpg"}"#);
        touch(&dir, "track.mp3", "x");
        let err = read_downloaded(&dir).unwrap_err().to_string();
        assert!(err.contains("missing thumbnail"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_thumbnail_in_metadata_is_allowed_without_cover_file() {
        let dir = temp_dir("thumb_absent");
        touch(&dir, "track.info.json",
            r#"{"id":"1","title":"t","uploader":"u","url":"https://x"}"#);
        touch(&dir, "track.mp3", "x");
        let dl = read_downloaded(&dir).expect("no-artwork track should pass validation");
        assert!(dl.cover.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_audio_is_reported() {
        let dir = temp_dir("audio_missing");
        touch(&dir, "track.info.json",
            r#"{"id":"1","title":"t","uploader":"u","url":"https://x"}"#);
        let err = read_downloaded(&dir).unwrap_err().to_string();
        assert!(err.contains("missing audio"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
