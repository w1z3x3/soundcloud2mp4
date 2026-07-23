//! Video encoder selection, capability detection and ffmpeg argument building.
//!
//! # Why detection has to be *functional*, not a feature-list lookup
//!
//! `ffmpeg -encoders` lists every encoder the binary was *compiled* with, which
//! says nothing about whether the hardware to run it exists. The common Windows
//! gyan/BtbN builds ship `h264_nvenc`, `h264_qsv` **and** `h264_amf` compiled in
//! on every machine — so a box with only an AMD card still "has" NVENC and QSV
//! according to the encoder list, and selecting one fails deep inside a
//! long render instead of at startup.
//!
//! So availability is decided in three steps, and only the last is trusted as a
//! yes:
//!
//! 1. Is the encoder compiled into this ffmpeg at all? (`-encoders`)
//! 2. Is a GPU of the right vendor actually present? (OS device query)
//! 3. Does a tiny real encode to `-f null` actually succeed?
//!
//! Anything short of a clean step 3 is reported as unavailable *with the reason*,
//! and the app falls back to CPU (`libx264`). Nothing is ever assumed.

use tokio_util::sync::CancellationToken;

use crate::utils::process::{run_capture, tool_command};

/// The encoder the user picked in Settings. `Auto` resolves at run time to the
/// best *available* hardware encoder, or CPU when none is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EncoderChoice {
    /// Pick the best available hardware encoder, else CPU.
    Auto,
    /// Force software `libx264`.
    Cpu,
    /// NVIDIA `h264_nvenc`.
    Nvenc,
    /// Intel Quick Sync `h264_qsv`.
    Qsv,
    /// AMD AMF `h264_amf`.
    Amf,
}

impl Default for EncoderChoice {
    fn default() -> Self {
        EncoderChoice::Auto
    }
}

impl EncoderChoice {
    pub const ALL: [EncoderChoice; 5] = [
        EncoderChoice::Auto,
        EncoderChoice::Cpu,
        EncoderChoice::Nvenc,
        EncoderChoice::Qsv,
        EncoderChoice::Amf,
    ];

    pub fn label(self) -> &'static str {
        match self {
            EncoderChoice::Auto => "Auto (recommended)",
            EncoderChoice::Cpu => "CPU (libx264)",
            EncoderChoice::Nvenc => "NVIDIA NVENC (h264_nvenc)",
            EncoderChoice::Qsv => "Intel Quick Sync (h264_qsv)",
            EncoderChoice::Amf => "AMD AMF (h264_amf)",
        }
    }

    /// The concrete hardware encoder this choice names, or `None` for
    /// `Auto`/`Cpu` (which are always selectable).
    pub fn kind(self) -> Option<EncoderKind> {
        match self {
            EncoderChoice::Auto | EncoderChoice::Cpu => None,
            EncoderChoice::Nvenc => Some(EncoderKind::Nvenc),
            EncoderChoice::Qsv => Some(EncoderKind::Qsv),
            EncoderChoice::Amf => Some(EncoderKind::Amf),
        }
    }
}

/// A concrete encoder actually used for a render — no `Auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    Cpu,
    Nvenc,
    Qsv,
    Amf,
}

impl EncoderKind {
    /// The three hardware encoders, in the priority `Auto` uses.
    pub const HARDWARE: [EncoderKind; 3] =
        [EncoderKind::Nvenc, EncoderKind::Qsv, EncoderKind::Amf];

    /// The ffmpeg `-c:v` codec name.
    pub fn codec(self) -> &'static str {
        match self {
            EncoderKind::Cpu => "libx264",
            EncoderKind::Nvenc => "h264_nvenc",
            EncoderKind::Qsv => "h264_qsv",
            EncoderKind::Amf => "h264_amf",
        }
    }

    /// GPU vendor whose hardware this encoder needs, if any.
    fn vendor(self) -> Option<Vendor> {
        match self {
            EncoderKind::Cpu => None,
            EncoderKind::Nvenc => Some(Vendor::Nvidia),
            EncoderKind::Qsv => Some(Vendor::Intel),
            EncoderKind::Amf => Some(Vendor::Amd),
        }
    }

    /// Short human name for progress display, e.g. "AMD AMF".
    pub fn short_label(self) -> &'static str {
        match self {
            EncoderKind::Cpu => "CPU (libx264)",
            EncoderKind::Nvenc => "NVIDIA NVENC",
            EncoderKind::Qsv => "Intel Quick Sync",
            EncoderKind::Amf => "AMD AMF",
        }
    }

    /// "AMD AMF (h264_amf)" — vendor plus codec, for logs and the UI.
    pub fn full_label(self) -> String {
        match self {
            EncoderKind::Cpu => "CPU (libx264)".to_string(),
            _ => format!("{} ({})", self.short_label(), self.codec()),
        }
    }

    /// The `-hwaccel` method to decode inputs with when hardware decoding is
    /// enabled. Frames are downloaded to system memory for the CPU filter graph
    /// (scale / xfade / drawtext), so this only offloads *decoding* — exactly
    /// the split that keeps compositing on the CPU while the GPU does the
    /// heavy H.264 work.
    pub fn hwaccel_decode(self) -> Option<&'static str> {
        match self {
            EncoderKind::Cpu => None,
            EncoderKind::Nvenc => Some("cuda"),
            EncoderKind::Qsv => Some("qsv"),
            // AMD has no decode-specific hwaccel; d3d11va is the Windows path.
            EncoderKind::Amf => Some("d3d11va"),
        }
    }
}

/// Which encode a given argument set is for. The three contexts have different
/// speed/quality trade-offs — a throwaway intermediate can be faster and larger
/// than the final file the user keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityTier {
    /// A per-track clip: a static cover with text, so `stillimage` tuning helps.
    Clip,
    /// A combine batch that will be re-encoded again — favour speed and fidelity.
    Intermediate,
    /// The final playlist video the user keeps — favour quality.
    Final,
}

/// A resolved encoder plus whether hardware decoding should be requested for its
/// inputs. Everything downstream builds ffmpeg arguments from this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedEncoder {
    pub kind: EncoderKind,
    /// Emit `-hwaccel <method>` before each input (combine stage only — the
    /// per-clip render decodes a still image, where it would not help).
    pub hardware_decode: bool,
}

impl ResolvedEncoder {
    /// Plain software encoding — the universal fallback and the test default.
    pub fn cpu() -> Self {
        ResolvedEncoder { kind: EncoderKind::Cpu, hardware_decode: false }
    }

    /// `-hwaccel <method>` args to place *before* an input, or empty when
    /// hardware decoding is off or unavailable for this encoder.
    pub fn decode_args(&self) -> Vec<String> {
        if !self.hardware_decode {
            return Vec::new();
        }
        match self.kind.hwaccel_decode() {
            Some(method) => vec!["-hwaccel".into(), method.into()],
            None => Vec::new(),
        }
    }

    /// The `-c:v ...` block (codec, preset/quality, pixel format) for `tier`.
    /// Audio is handled separately and is never touched here.
    pub fn video_args(&self, tier: QualityTier) -> Vec<String> {
        let v: Vec<&str> = match (self.kind, tier) {
            // ---- CPU (libx264): unchanged from the original pipeline ---------
            (EncoderKind::Cpu, QualityTier::Clip) => {
                vec!["-c:v", "libx264", "-preset", "medium", "-tune", "stillimage", "-crf", "18"]
            }
            (EncoderKind::Cpu, QualityTier::Intermediate) => {
                vec!["-c:v", "libx264", "-preset", "veryfast", "-crf", "16"]
            }
            (EncoderKind::Cpu, QualityTier::Final) => {
                vec!["-c:v", "libx264", "-preset", "medium", "-crf", "18"]
            }

            // ---- NVIDIA NVENC ------------------------------------------------
            (EncoderKind::Nvenc, QualityTier::Intermediate) => {
                vec!["-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", "20", "-b:v", "0"]
            }
            (EncoderKind::Nvenc, _) => {
                vec!["-c:v", "h264_nvenc", "-preset", "p5", "-rc", "vbr", "-cq", "22", "-b:v", "0"]
            }

            // ---- Intel Quick Sync -------------------------------------------
            (EncoderKind::Qsv, QualityTier::Intermediate) => {
                vec!["-c:v", "h264_qsv", "-global_quality", "20"]
            }
            (EncoderKind::Qsv, _) => {
                vec!["-c:v", "h264_qsv", "-global_quality", "22"]
            }

            // ---- AMD AMF -----------------------------------------------------
            (EncoderKind::Amf, QualityTier::Intermediate) => {
                vec!["-c:v", "h264_amf", "-quality", "speed", "-rc", "cqp", "-qp_i", "20", "-qp_p", "20"]
            }
            (EncoderKind::Amf, _) => {
                vec!["-c:v", "h264_amf", "-quality", "quality", "-rc", "cqp", "-qp_i", "22", "-qp_p", "22"]
            }
        };
        let mut args: Vec<String> = v.into_iter().map(String::from).collect();
        // Every path outputs 8-bit 4:2:0 for maximum player compatibility.
        args.extend(["-pix_fmt".into(), "yuv420p".into()]);
        args
    }
}

/// GPU vendors we can map to a hardware encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vendor {
    Nvidia,
    Intel,
    Amd,
}

impl Vendor {
    fn matches(self, gpu_name: &str) -> bool {
        let n = gpu_name.to_lowercase();
        match self {
            Vendor::Nvidia => n.contains("nvidia") || n.contains("geforce") || n.contains("quadro"),
            Vendor::Intel => n.contains("intel"),
            Vendor::Amd => n.contains("amd") || n.contains("radeon"),
        }
    }

    fn human(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Intel => "Intel",
            Vendor::Amd => "AMD",
        }
    }
}

/// Whether a hardware encoder can actually be used, and if not, why — the
/// "why" is shown next to the disabled option in Settings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Availability {
    Available,
    Unavailable(String),
}

impl Availability {
    pub fn is_available(&self) -> bool {
        matches!(self, Availability::Available)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Availability::Available => None,
            Availability::Unavailable(r) => Some(r),
        }
    }
}

/// The result of probing the machine at startup: which GPUs are present and
/// whether each hardware encoder is genuinely usable.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EncoderSupport {
    /// GPU device names as the OS reports them, for display and messages.
    pub gpus: Vec<String>,
    pub nvenc: Availability,
    pub qsv: Availability,
    pub amf: Availability,
}

impl Default for Availability {
    fn default() -> Self {
        Availability::Unavailable("not yet detected".into())
    }
}

impl EncoderSupport {
    pub fn availability(&self, kind: EncoderKind) -> Availability {
        match kind {
            EncoderKind::Cpu => Availability::Available,
            EncoderKind::Nvenc => self.nvenc.clone(),
            EncoderKind::Qsv => self.qsv.clone(),
            EncoderKind::Amf => self.amf.clone(),
        }
    }

    /// Availability of a user-facing choice. `Auto` and `Cpu` are always usable.
    pub fn choice_availability(&self, choice: EncoderChoice) -> Availability {
        match choice.kind() {
            None => Availability::Available,
            Some(kind) => self.availability(kind),
        }
    }

    /// Resolve a setting to a concrete encoder plus a note when it fell back
    /// (e.g. the user chose NVENC but it is not available, so CPU is used).
    pub fn resolve(
        &self,
        choice: EncoderChoice,
        hardware_decode: bool,
    ) -> (ResolvedEncoder, Option<String>) {
        let (kind, note) = match choice {
            EncoderChoice::Cpu => (EncoderKind::Cpu, None),
            EncoderChoice::Auto => {
                let picked = EncoderKind::HARDWARE
                    .into_iter()
                    .find(|k| self.availability(*k).is_available())
                    .unwrap_or(EncoderKind::Cpu);
                (picked, None)
            }
            EncoderChoice::Nvenc | EncoderChoice::Qsv | EncoderChoice::Amf => {
                let kind = choice.kind().unwrap();
                match self.availability(kind) {
                    Availability::Available => (kind, None),
                    Availability::Unavailable(reason) => (
                        EncoderKind::Cpu,
                        Some(format!(
                            "{} is not available ({reason}); using CPU (libx264) instead.",
                            kind.short_label()
                        )),
                    ),
                }
            }
        };
        // Hardware decoding is meaningless for the CPU encoder.
        let hw = hardware_decode && kind != EncoderKind::Cpu && kind.hwaccel_decode().is_some();
        (ResolvedEncoder { kind, hardware_decode: hw }, note)
    }
}

/// Where the last detection result is cached, so the UI can show encoder
/// availability instantly on the next launch while a fresh probe runs in the
/// background.
pub fn cache_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("soundcloud2mp4")
        .join("encoders.json")
}

/// Load the cached detection result, if any. Advisory only — a background probe
/// always refreshes it, so a stale cache self-corrects within a second of
/// launch.
pub fn load_cache() -> Option<EncoderSupport> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist a detection result for the next launch (best-effort).
pub fn save_cache(support: &EncoderSupport) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(support) {
        let _ = std::fs::write(&path, text);
    }
}

/// Probe ffmpeg + the OS for encoder support. Runs at startup and whenever the
/// ffmpeg path changes. Never fails: on any error it returns "unavailable" with
/// a reason, so the app degrades to CPU rather than refusing to start. The
/// result is cached for the next launch.
pub async fn detect(ffmpeg: &str) -> EncoderSupport {
    let token = CancellationToken::new();
    let compiled = compiled_in_encoders(ffmpeg, &token).await;
    let gpus = detect_gpus(&token).await;

    let mut support = EncoderSupport { gpus: gpus.clone(), ..Default::default() };
    support.nvenc = probe(ffmpeg, EncoderKind::Nvenc, &compiled, &gpus, &token).await;
    support.qsv = probe(ffmpeg, EncoderKind::Qsv, &compiled, &gpus, &token).await;
    support.amf = probe(ffmpeg, EncoderKind::Amf, &compiled, &gpus, &token).await;
    save_cache(&support);
    support
}

/// Names of every encoder compiled into this ffmpeg (`-encoders`). Compiled-in
/// is necessary but not sufficient — see the module docs.
async fn compiled_in_encoders(ffmpeg: &str, token: &CancellationToken) -> Vec<String> {
    let mut cmd = tool_command(ffmpeg);
    cmd.args(["-hide_banner", "-encoders"]);
    match run_capture(cmd, ffmpeg, token, None).await {
        Ok((true, out, _)) => parse_encoder_names(&out),
        _ => Vec::new(),
    }
}

/// Extract encoder names from `ffmpeg -encoders` output. Each encoder line is
/// ` FLAGS name  description`; the name is the second whitespace field.
fn parse_encoder_names(out: &str) -> Vec<String> {
    out.lines()
        .skip_while(|l| !l.contains("------"))
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect()
}

/// GPU device names from the OS. Best-effort: an empty list means "could not
/// tell", which is treated as "do not veto on vendor" so the functional probe
/// still gets a chance.
async fn detect_gpus(token: &CancellationToken) -> Vec<String> {
    #[cfg(windows)]
    {
        // wmic first (fast, present on most Windows), PowerShell CIM as a
        // fallback for the newer installs where wmic has been removed.
        let mut wmic = tool_command("wmic");
        wmic.args(["path", "win32_VideoController", "get", "name"]);
        if let Ok((true, out, _)) = run_capture(wmic, "wmic", token, None).await {
            let names = parse_gpu_lines(&out);
            if !names.is_empty() {
                return names;
            }
        }
        let mut ps = tool_command("powershell");
        ps.args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name",
        ]);
        if let Ok((true, out, _)) = run_capture(ps, "powershell", token, None).await {
            return parse_gpu_lines(&out);
        }
        Vec::new()
    }
    #[cfg(not(windows))]
    {
        let _ = token;
        Vec::new()
    }
}

/// Clean the multi-line device-name output of wmic / PowerShell into a list,
/// dropping the header row and blanks.
fn parse_gpu_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("name"))
        .map(str::to_string)
        .collect()
}

/// Decide one hardware encoder's availability via the three-step gate.
async fn probe(
    ffmpeg: &str,
    kind: EncoderKind,
    compiled: &[String],
    gpus: &[String],
    token: &CancellationToken,
) -> Availability {
    let codec = kind.codec();

    // 1. Compiled into this ffmpeg at all?
    if !compiled.iter().any(|e| e == codec) {
        return Availability::Unavailable(format!("this ffmpeg build has no {codec} support"));
    }

    // 2. Is the matching GPU present? Only veto when GPU detection actually
    //    returned something — an empty list means we could not tell, so we let
    //    the functional probe be the judge.
    if let Some(vendor) = kind.vendor() {
        if !gpus.is_empty() && !gpus.iter().any(|g| vendor.matches(g)) {
            return Availability::Unavailable(format!(
                "{} GPU not detected or ffmpeg lacks {codec} support",
                vendor.human()
            ));
        }
    }

    // 3. The only real yes: a tiny actual encode succeeds.
    match test_encode(ffmpeg, codec, token).await {
        Ok(()) => Availability::Available,
        Err(reason) => Availability::Unavailable(reason),
    }
}

/// Encode two frames of a synthetic source to `-f null` with `codec`. Succeeds
/// only when the driver, hardware and ffmpeg genuinely agree — the definitive
/// availability test. Returns a short reason on failure.
async fn test_encode(ffmpeg: &str, codec: &str, token: &CancellationToken) -> Result<(), String> {
    let mut cmd = tool_command(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=256x144:r=2:d=0.1",
        "-c:v",
        codec,
        "-pix_fmt",
        "yuv420p",
        "-f",
        "null",
        "-",
    ]);
    match run_capture(cmd, ffmpeg, token, None).await {
        Ok((true, _, _)) => Ok(()),
        Ok((false, _, stderr)) => Err(short_reason(&stderr)),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Pull the most telling line out of an ffmpeg failure for the UI tooltip.
fn short_reason(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("frame=") && !l.starts_with("["))
        .or_else(|| stderr.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("test encode failed");
    let line = line.strip_prefix("Error").map(|_| line).unwrap_or(line);
    line.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_args_match_the_original_pipeline() {
        let e = ResolvedEncoder::cpu();
        assert_eq!(
            e.video_args(QualityTier::Clip).join(" "),
            "-c:v libx264 -preset medium -tune stillimage -crf 18 -pix_fmt yuv420p"
        );
        assert_eq!(
            e.video_args(QualityTier::Intermediate).join(" "),
            "-c:v libx264 -preset veryfast -crf 16 -pix_fmt yuv420p"
        );
        assert_eq!(
            e.video_args(QualityTier::Final).join(" "),
            "-c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p"
        );
        // CPU never asks for hardware decode.
        assert!(e.decode_args().is_empty());
    }

    #[test]
    fn gpu_args_use_the_right_codec_and_quality() {
        for (kind, codec) in [
            (EncoderKind::Nvenc, "h264_nvenc"),
            (EncoderKind::Qsv, "h264_qsv"),
            (EncoderKind::Amf, "h264_amf"),
        ] {
            let e = ResolvedEncoder { kind, hardware_decode: true };
            let final_args = e.video_args(QualityTier::Final).join(" ");
            assert!(final_args.contains(&format!("-c:v {codec}")), "{final_args}");
            assert!(final_args.ends_with("-pix_fmt yuv420p"), "{final_args}");
        }
        // Spec examples.
        assert!(ResolvedEncoder { kind: EncoderKind::Nvenc, hardware_decode: false }
            .video_args(QualityTier::Final)
            .join(" ")
            .contains("-preset p5"));
        assert!(ResolvedEncoder { kind: EncoderKind::Qsv, hardware_decode: false }
            .video_args(QualityTier::Final)
            .join(" ")
            .contains("-global_quality 22"));
        assert!(ResolvedEncoder { kind: EncoderKind::Amf, hardware_decode: false }
            .video_args(QualityTier::Final)
            .join(" ")
            .contains("-quality quality"));
    }

    #[test]
    fn decode_args_follow_encoder_and_toggle() {
        assert_eq!(
            ResolvedEncoder { kind: EncoderKind::Nvenc, hardware_decode: true }.decode_args(),
            vec!["-hwaccel".to_string(), "cuda".to_string()]
        );
        assert_eq!(
            ResolvedEncoder { kind: EncoderKind::Amf, hardware_decode: true }.decode_args(),
            vec!["-hwaccel".to_string(), "d3d11va".to_string()]
        );
        assert!(ResolvedEncoder { kind: EncoderKind::Qsv, hardware_decode: false }
            .decode_args()
            .is_empty());
    }

    #[test]
    fn auto_prefers_hardware_then_falls_back_to_cpu() {
        // Nothing available -> CPU.
        let none = EncoderSupport::default();
        assert_eq!(none.resolve(EncoderChoice::Auto, true).0.kind, EncoderKind::Cpu);

        // AMF available -> Auto picks it.
        let amd = EncoderSupport {
            amf: Availability::Available,
            ..Default::default()
        };
        let (resolved, note) = amd.resolve(EncoderChoice::Auto, true);
        assert_eq!(resolved.kind, EncoderKind::Amf);
        assert!(resolved.hardware_decode);
        assert!(note.is_none());

        // NVENC preferred over AMF when both are available.
        let both = EncoderSupport {
            nvenc: Availability::Available,
            amf: Availability::Available,
            ..Default::default()
        };
        assert_eq!(both.resolve(EncoderChoice::Auto, false).0.kind, EncoderKind::Nvenc);
    }

    #[test]
    fn choosing_an_unavailable_encoder_falls_back_with_a_note() {
        let support = EncoderSupport {
            nvenc: Availability::Unavailable("NVIDIA GPU not detected".into()),
            ..Default::default()
        };
        let (resolved, note) = support.resolve(EncoderChoice::Nvenc, true);
        assert_eq!(resolved.kind, EncoderKind::Cpu);
        assert!(note.unwrap().contains("NVIDIA GPU not detected"));
    }

    #[test]
    fn parse_encoders_finds_names_after_the_divider() {
        let out = "Encoders:\n V..... = Video\n ------\n V....D libx264 desc\n V....D h264_amf desc\n";
        let names = parse_encoder_names(out);
        assert!(names.contains(&"libx264".to_string()));
        assert!(names.contains(&"h264_amf".to_string()));
    }

    #[test]
    fn parse_gpu_lines_drops_header_and_blanks() {
        let out = "Name\n\nAMD Radeon Graphics\n";
        assert_eq!(parse_gpu_lines(out), vec!["AMD Radeon Graphics".to_string()]);
    }

    #[test]
    fn vendor_matching_is_case_insensitive() {
        assert!(Vendor::Amd.matches("AMD Radeon Graphics"));
        assert!(Vendor::Nvidia.matches("NVIDIA GeForce RTX"));
        assert!(Vendor::Intel.matches("Intel(R) UHD Graphics"));
        assert!(!Vendor::Nvidia.matches("AMD Radeon Graphics"));
    }
}
