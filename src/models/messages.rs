use super::track::{MetaFailure, Track, TrackMetadata, TrackStatus};

/// Messages sent from background workers to the GUI thread.
#[derive(Debug)]
pub enum WorkerMsg {
    Log(String),
    /// Result of loading a playlist: (playlist title, placeholder tracks) or an error.
    Playlist(Result<(String, Vec<Track>), String>),
    /// Second-stage metadata for one track (by index), or why it failed —
    /// including whether retrying could ever help.
    TrackMeta(usize, Result<TrackMetadata, MetaFailure>),
    /// The initial metadata enrichment pass finished. `rate_limited` counts
    /// the failures that looked like rate limiting; when it is non-zero the
    /// GUI starts a retry pass automatically. `permanent` counts tracks that
    /// are gone for good (deleted, private, region-locked) and are therefore
    /// never retried; `restricted` counts tracks SoundCloud withheld from an
    /// anonymous client, which are not retried automatically but may succeed
    /// with a cookies file.
    MetaLoadFinished {
        cancelled: bool,
        failed: usize,
        rate_limited: usize,
        permanent: usize,
        restricted: usize,
    },
    /// A retry for this track has started — put the row back to "fetching".
    MetaRetrying(usize),
    /// Retry pass progress. `remaining` counts tracks not yet resolved.
    MetaRetryProgress { remaining: usize, total: usize },
    /// Retry pass finished; counts for the completion summary. `gave_up` means
    /// the back-off ladder topped out and the pass stopped early rather than
    /// grinding through a hard rate limit. `permanent` counts tracks the pass
    /// found to be gone for good, which will not be offered for retry again.
    MetaRetryFinished {
        recovered: usize,
        still_failed: usize,
        permanent: usize,
        restricted: usize,
        cancelled: bool,
        gave_up: bool,
    },
    TrackStatus(usize, TrackStatus),
    Progress {
        phase: String,
        frac: f32,
    },
    Finished {
        ok: usize,
        failed: usize,
        cancelled: bool,
    },
    Tools {
        ffmpeg: Option<String>,
        ytdlp: Option<String>,
    },
    /// Result of probing which video encoders this machine can actually use.
    Encoders(crate::video::encoder::EncoderSupport),
    /// The encoder a conversion is actually using, for the progress display
    /// (e.g. "AMD AMF (h264_amf)"). Sent when a conversion starts.
    EncoderActive(String),
    /// A finished encoder benchmark: which encoder, how long a 30 s sample took,
    /// the average FPS, and the output file size in bytes.
    BenchmarkDone(Result<BenchmarkResult, String>),
    /// Result of probing the selected browser's cookie jar.
    CookieStatus(crate::downloader::cookies::CookieStatus),
    /// A status/output line from the tool setup assistant.
    SetupProgress(String),
    /// The setup assistant finished (successfully or not).
    SetupDone {
        success: bool,
        message: String,
        /// True when installation failed because admin rights are required.
        needs_elevation: bool,
    },
}

/// One encoder benchmark outcome, shown in the Encoding settings so CPU and GPU
/// can be compared before committing to a multi-hour render.
#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    /// Encoder label, e.g. "AMD AMF (h264_amf)".
    pub encoder: String,
    /// Wall-clock seconds to encode the 30 s sample.
    pub elapsed_s: f64,
    /// Average frames per second over the sample.
    pub fps: f64,
    /// Output file size in bytes.
    pub size_bytes: u64,
}

/// Cloneable sender that also wakes the GUI so messages show up immediately.
#[derive(Clone)]
pub struct Tx {
    pub tx: std::sync::mpsc::Sender<WorkerMsg>,
    pub ctx: egui::Context,
}

impl Tx {
    pub fn send(&self, msg: WorkerMsg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }

    pub fn log(&self, line: impl Into<String>) {
        self.send(WorkerMsg::Log(line.into()));
    }
}
