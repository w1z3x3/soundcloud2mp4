use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::models::messages::Tx;

/// Build a Command that never pops up a console window on Windows.
pub fn tool_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Human-readable command line for logging, e.g. `yt-dlp --flat-playlist -J URL`.
pub fn describe(cmd: &Command) -> String {
    let std = cmd.as_std();
    let mut parts = vec![std.get_program().to_string_lossy().into_owned()];
    for arg in std.get_args() {
        let a = arg.to_string_lossy();
        if a.contains(' ') {
            parts.push(format!("\"{a}\""));
        } else {
            parts.push(a.into_owned());
        }
    }
    parts.join(" ")
}

/// Append a line to a debug log file (best-effort; creates parent dirs).
pub fn append_log(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn spawn_err(program: &str, e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow!("'{program}' was not found. Install it and/or set its path in Settings.")
    } else {
        anyhow!("failed to start '{program}': {e}")
    }
}

/// Run to completion, capturing stdout/stderr fully. Kills the child if
/// cancelled. The executed command line and exit code are appended to
/// `log_file` when given.
///
/// Thin wrapper over [`run_capture_status`] for callers that only need to know
/// whether the tool succeeded.
pub async fn run_capture(
    cmd: Command,
    program: &str,
    token: &CancellationToken,
    log_file: Option<&Path>,
) -> Result<(bool, String, String)> {
    let (status, out, err) = run_capture_status(cmd, program, token, log_file).await?;
    Ok((status.success(), out, err))
}

/// Same as [`run_capture`] but hands back the raw `ExitStatus`, so callers can
/// log/classify the actual exit code (yt-dlp uses it to distinguish "no such
/// track" from a transient network/rate-limit failure).
pub async fn run_capture_status(
    mut cmd: Command,
    program: &str,
    token: &CancellationToken,
    log_file: Option<&Path>,
) -> Result<(std::process::ExitStatus, String, String)> {
    let cmdline = describe(&cmd);
    tracing::info!("running: {cmdline}");
    if let Some(log) = log_file {
        append_log(log, &format!("\n=== Running: {cmdline}"));
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| spawn_err(program, e))?;

    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });

    tokio::select! {
        _ = token.cancelled() => {
            let _ = child.kill().await;
            bail!("cancelled");
        }
        status = child.wait() => {
            let status = status.context("waiting for child")?;
            let out = String::from_utf8_lossy(&out_task.await.unwrap_or_default()).into_owned();
            let err = String::from_utf8_lossy(&err_task.await.unwrap_or_default()).into_owned();
            if let Some(log) = log_file {
                append_log(log, &format!(
                    "=== exit: {status} | stdout: {} bytes | stderr: {} bytes", out.len(), err.len()
                ));
                if !err.trim().is_empty() {
                    append_log(log, &format!("--- stderr ---\n{}", err.trim()));
                }
            }
            Ok((status, out, err))
        }
    }
}

/// Run to completion, streaming every output line to the GUI log in real time
/// (and to `log_file` when given). On failure the error contains the last few
/// stderr lines so the GUI can show the actual reason.
pub async fn run_streaming(
    mut cmd: Command,
    program: &str,
    token: &CancellationToken,
    tx: &Tx,
    prefix: &str,
    log_file: Option<&Path>,
) -> Result<()> {
    let cmdline = describe(&cmd);
    tracing::info!("running: {cmdline}");
    tx.log(format!("Running: {cmdline}"));
    if let Some(log) = log_file {
        append_log(log, &format!("\n=== Running: {cmdline}"));
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| spawn_err(program, e))?;

    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));

    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    let tx1 = tx.clone();
    let p1 = prefix.to_string();
    let log1 = log_file.map(Path::to_path_buf);
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() {
                if let Some(log) = &log1 {
                    append_log(log, &line);
                }
                tx1.log(format!("[{p1}] {line}"));
            }
        }
    });

    let tx2 = tx.clone();
    let p2 = prefix.to_string();
    let tail2 = tail.clone();
    let log2 = log_file.map(Path::to_path_buf);
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            {
                let mut t = tail2.lock().unwrap();
                t.push_back(trimmed.to_string());
                if t.len() > 8 {
                    t.pop_front();
                }
            }
            if let Some(log) = &log2 {
                append_log(log, trimmed);
            }
            tx2.log(format!("[{p2}] {trimmed}"));
        }
    });

    let status = tokio::select! {
        _ = token.cancelled() => {
            let _ = child.kill().await;
            bail!("cancelled");
        }
        status = child.wait() => status.context("waiting for child")?,
    };
    let _ = out_task.await;
    let _ = err_task.await;

    if let Some(log) = log_file {
        append_log(log, &format!("=== exit: {status}"));
    }

    if status.success() {
        Ok(())
    } else {
        let tail = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        bail!("{program} exited with code {code}\n\nstderr:\n{tail}")
    }
}

/// Probe a tool's version (`--version` for yt-dlp, `-version` for ffmpeg).
pub async fn version_of(program: &str, version_flag: &str) -> Option<String> {
    let mut cmd = tool_command(program);
    cmd.arg(version_flag);
    let token = CancellationToken::new();
    match run_capture(cmd, program, &token, None).await {
        Ok((true, out, _)) => out.lines().next().map(|s| s.trim().to_string()),
        _ => None,
    }
}
