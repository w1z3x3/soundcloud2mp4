//! Automatic ffmpeg / yt-dlp setup assistant.
//!
//! On Windows this drives `winget` to install the missing tools, streams
//! progress to the GUI, refreshes the process PATH and re-checks availability.
//! Nothing here touches the conversion pipeline — it only gets the external
//! tools onto the machine.

use tokio_util::sync::CancellationToken;

use crate::models::messages::{Tx, WorkerMsg};
use crate::utils::process::{run_capture, tool_command};

/// winget package IDs.
const FFMPEG_ID: &str = "Gyan.FFmpeg";
const YTDLP_ID: &str = "yt-dlp.yt-dlp";

/// Is `winget` present on this machine? Windows only.
pub fn winget_available() -> bool {
    if !cfg!(windows) {
        return false;
    }
    let mut cmd = std::process::Command::new("winget");
    cmd.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    matches!(cmd.status(), Ok(s) if s.success())
}

/// Manual, copy-pasteable install instructions for the current platform.
pub fn manual_instructions() -> String {
    if cfg!(windows) {
        "Install manually, then click \"recheck\":\n\
         • ffmpeg:  winget install Gyan.FFmpeg   (or download from https://ffmpeg.org)\n\
         • yt-dlp:  winget install yt-dlp.yt-dlp (or download from https://github.com/yt-dlp/yt-dlp)"
            .into()
    } else if cfg!(target_os = "macos") {
        "Install manually with Homebrew, then click \"recheck\":\n\
         • brew install ffmpeg\n\
         • brew install yt-dlp"
            .into()
    } else {
        "Install manually with your package manager, then click \"recheck\":\n\
         • sudo apt install ffmpeg   (or your distro's equivalent)\n\
         • sudo apt install yt-dlp   (or: pipx install yt-dlp)"
            .into()
    }
}

/// Run one `winget install`. On failure returns (message, needs_elevation).
async fn run_winget(id: &str, label: &str, tx: &Tx) -> Result<(), (String, bool)> {
    tx.send(WorkerMsg::SetupProgress(format!(
        "Installing {label}  (winget install {id}) — this can take a minute..."
    )));

    let mut cmd = tool_command("winget");
    cmd.args([
        "install",
        "--id",
        id,
        "-e",
        "--source",
        "winget",
        "--accept-source-agreements",
        "--accept-package-agreements",
        "--disable-interactivity",
    ]);

    let token = CancellationToken::new();
    match run_capture(cmd, "winget", &token, None).await {
        Ok((true, out, err)) => {
            for line in out.lines().chain(err.lines()) {
                let t = line.trim();
                if !t.is_empty() {
                    tx.send(WorkerMsg::SetupProgress(format!("  {t}")));
                }
            }
            tx.send(WorkerMsg::SetupProgress(format!("{label} installed.")));
            Ok(())
        }
        Ok((false, out, err)) => {
            let combined = format!("{out}\n{err}");
            let lower = combined.to_lowercase();
            let needs_elevation = lower.contains("elevat")
                || lower.contains("administrator")
                || lower.contains("access is denied")
                || lower.contains("0x80073cf9");
            let tail: Vec<String> = combined
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .rev()
                .take(5)
                .map(str::to_string)
                .collect();
            let msg = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
            Err((
                if msg.is_empty() {
                    format!("winget failed to install {label}")
                } else {
                    msg
                },
                needs_elevation,
            ))
        }
        Err(e) => {
            let s = e.to_string();
            let needs_elevation = s.to_lowercase().contains("elevat");
            Err((format!("could not run winget: {s}"), needs_elevation))
        }
    }
}

/// Refresh the current process's PATH from the Windows registry so tools that
/// were just installed become resolvable without restarting the app.
#[cfg(windows)]
async fn refresh_path(tx: &Tx) {
    let mut cmd = tool_command("powershell");
    cmd.args([
        "-NoProfile",
        "-Command",
        "[Environment]::GetEnvironmentVariable('Path','Machine') + ';' + \
         [Environment]::GetEnvironmentVariable('Path','User')",
    ]);
    let token = CancellationToken::new();
    if let Ok((true, out, _)) = run_capture(cmd, "powershell", &token, None).await {
        let combined = out.trim();
        if !combined.is_empty() {
            std::env::set_var("PATH", combined);
            tx.send(WorkerMsg::SetupProgress("PATH refreshed.".into()));
            return;
        }
    }
    tx.send(WorkerMsg::SetupProgress(
        "Could not refresh PATH automatically (a restart may be needed).".into(),
    ));
}

#[cfg(not(windows))]
async fn refresh_path(_tx: &Tx) {}

/// Install the requested tools, then refresh PATH and re-probe availability.
/// All feedback is delivered via `WorkerMsg` on `tx`.
pub async fn install_missing(
    install_ffmpeg: bool,
    install_ytdlp: bool,
    ffmpeg_path: String,
    ytdlp_path: String,
    tx: Tx,
) {
    let mut errors: Vec<String> = Vec::new();
    let mut needs_elevation = false;

    if install_ffmpeg {
        if let Err((msg, elev)) = run_winget(FFMPEG_ID, "ffmpeg", &tx).await {
            errors.push(format!("ffmpeg: {msg}"));
            needs_elevation |= elev;
        }
    }
    if install_ytdlp {
        if let Err((msg, elev)) = run_winget(YTDLP_ID, "yt-dlp", &tx).await {
            errors.push(format!("yt-dlp: {msg}"));
            needs_elevation |= elev;
        }
    }

    tx.send(WorkerMsg::SetupProgress("Refreshing PATH...".into()));
    refresh_path(&tx).await;

    tx.send(WorkerMsg::SetupProgress("Re-checking tools...".into()));
    let (ff, yt) = tokio::join!(
        crate::utils::process::version_of(&ffmpeg_path, "-version"),
        crate::utils::process::version_of(&ytdlp_path, "--version"),
    );
    tx.send(WorkerMsg::Tools {
        ffmpeg: ff.clone(),
        ytdlp: yt.clone().map(|v| format!("yt-dlp {v}")),
    });

    let ffmpeg_ok = !install_ffmpeg || ff.is_some();
    let ytdlp_ok = !install_ytdlp || yt.is_some();
    let success = ffmpeg_ok && ytdlp_ok && errors.is_empty();

    let message = if success {
        "Tools installed and detected successfully.".to_string()
    } else if !errors.is_empty() {
        format!(
            "{}\n\n{}",
            errors.join("\n\n"),
            manual_instructions()
        )
    } else {
        format!(
            "Installation finished but the tools were still not detected.\n\n{}",
            manual_instructions()
        )
    };

    tx.send(WorkerMsg::SetupDone {
        success,
        message,
        needs_elevation: needs_elevation && !success,
    });
}

/// Relaunch this executable with a UAC elevation prompt, then exit.
pub fn relaunch_as_admin() {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let mut cmd = std::process::Command::new("powershell");
            cmd.args([
                "-NoProfile",
                "-Command",
                &format!("Start-Process -FilePath '{}' -Verb RunAs", exe.display()),
            ]);
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
            let _ = cmd.spawn();
        }
    }
    std::process::exit(0);
}
