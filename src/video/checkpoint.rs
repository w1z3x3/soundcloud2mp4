//! `resume.json` — an explicit, human-readable record of a combined export's
//! progress, written into the `.work` directory as the run proceeds.
//!
//! # Why a checkpoint file on top of ffprobe-validated reuse
//!
//! The resume system already reuses any clip or batch that exists and probes
//! cleanly, so it can recover with no checkpoint at all (and still does, for
//! `.work` folders produced before this file existed). The checkpoint adds an
//! *explicit* statement of what finished and how far the run got, so a resume
//! reports a definite state ("stage: combine, 493 clips, 3 batches done, encoder
//! h264_amf") instead of inferring it, and records which encoder produced the
//! artifacts.
//!
//! It is deliberately **advisory, not authoritative**: every artifact it names
//! is still ffprobe-validated before reuse. That is what makes the exact failure
//! that motivated it — a crash *while a batch was being written* — safe: the
//! half-written batch was never recorded here (entries are added only after the
//! ffmpeg pass succeeds), and even a checkpoint corrupted mid-save cannot cause a
//! bad artifact to be trusted, because the probe is the final word.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// File name inside the `.work` directory.
pub const CHECKPOINT_FILE: &str = "resume.json";

/// Progress record for a combined export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Checkpoint {
    /// Playlist name, for display when resuming.
    pub playlist: String,
    /// ffmpeg codec the artifacts were produced with, e.g. "h264_amf".
    pub encoder: String,
    /// "render" while clips are being produced, "combine" during the ladder,
    /// "done" once the final video exists.
    pub stage: String,
    /// Track indices whose `clip_XXXX.mp4` finished.
    pub completed_clips: Vec<usize>,
    /// Batch output names (e.g. "batch_r1_0000.mkv") that finished.
    pub completed_batches: Vec<String>,
}

impl Checkpoint {
    pub fn path(workdir: &Path) -> PathBuf {
        workdir.join(CHECKPOINT_FILE)
    }

    /// Load the checkpoint from `workdir`, or `None` when absent/unreadable.
    pub fn load(workdir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(workdir)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Load if present, else a fresh empty checkpoint.
    pub fn load_or_new(workdir: &Path) -> Self {
        Self::load(workdir).unwrap_or_default()
    }

    /// Best-effort write. A failed write never fails the export — the checkpoint
    /// is advisory and the ffprobe-validated reuse still works without it.
    pub fn save(&self, workdir: &Path) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(workdir), text);
        }
    }

    pub fn set_stage(&mut self, workdir: &Path, stage: &str) {
        self.stage = stage.to_string();
        self.save(workdir);
    }

    /// Record a finished clip and persist. Only ever called after the clip is
    /// on disk and validated.
    pub fn record_clip(&mut self, workdir: &Path, index: usize) {
        if !self.completed_clips.contains(&index) {
            self.completed_clips.push(index);
        }
        self.save(workdir);
    }

    /// Record a finished batch and persist. Only ever called after the ffmpeg
    /// pass that produced it succeeded.
    pub fn record_batch(&mut self, workdir: &Path, name: &str) {
        if !self.completed_batches.iter().any(|b| b == name) {
            self.completed_batches.push(name.to_string());
        }
        self.save(workdir);
    }

    pub fn has_batch(&self, name: &str) -> bool {
        self.completed_batches.iter().any(|b| b == name)
    }

    /// True when the checkpoint carries progress worth reporting on resume.
    pub fn has_progress(&self) -> bool {
        !self.completed_clips.is_empty() || !self.completed_batches.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_dedups() {
        let dir = std::env::temp_dir().join(format!("ckpt_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let mut cp = Checkpoint::load_or_new(&dir);
        cp.playlist = "My List".into();
        cp.encoder = "h264_amf".into();
        cp.set_stage(&dir, "render");
        cp.record_clip(&dir, 0);
        cp.record_clip(&dir, 0); // duplicate ignored
        cp.record_clip(&dir, 1);
        cp.set_stage(&dir, "combine");
        cp.record_batch(&dir, "batch_r1_0000.mkv");

        let loaded = Checkpoint::load(&dir).expect("checkpoint should load");
        assert_eq!(loaded.playlist, "My List");
        assert_eq!(loaded.encoder, "h264_amf");
        assert_eq!(loaded.stage, "combine");
        assert_eq!(loaded.completed_clips, vec![0, 1]);
        assert!(loaded.has_batch("batch_r1_0000.mkv"));
        assert!(loaded.has_progress());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_checkpoint_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join("ckpt_absent_dir_xyz");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(Checkpoint::load(&dir).is_none());
        assert!(!Checkpoint::load_or_new(&dir).has_progress());
    }
}
