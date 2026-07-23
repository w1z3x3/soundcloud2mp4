//! SoundCloud authentication via browser cookies.
//!
//! Some tracks' plain MP3 streams are served only to a signed-in session; a
//! logged-out client gets a 404 on the transcoding and yt-dlp then reports the
//! track as "DRM protected" (see [`super::ytdlp::RESTRICTED_FAILURES`]). Rather
//! than asking the user to export a `cookies.txt` — which browsers increasingly
//! export as JSON, a format yt-dlp rejects — the app points yt-dlp straight at
//! the browser with `--cookies-from-browser`.
//!
//! # Chromium on Windows usually cannot be read
//!
//! Since Chrome 127 the cookie database is sealed with **app-bound
//! encryption**, and yt-dlp fails with `Failed to decrypt with DPAPI`
//! ([yt-dlp #10927](https://github.com/yt-dlp/yt-dlp/issues/10927)). In practice
//! Chromium-based browsers (Chrome, Edge, Brave) all fail this way on Windows
//! while Firefox reads fine. Closing the browser does not help — the key is held
//! by the OS for that application, so this is not a file-lock problem. The status
//! line says so plainly instead of sending the user round a loop of closing tabs.

use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::config::settings::CookieBrowser;
use crate::utils::process::{run_capture, tool_command};

/// Where yt-dlp should get cookies from for one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieSource {
    /// Run logged out, as the app always did.
    None,
    /// `--cookies-from-browser <name>`.
    Browser(CookieBrowser),
    /// `--cookies <file>`; the escape hatch when a browser cannot be read.
    File(PathBuf),
}

impl CookieSource {
    /// Add the right flag to a yt-dlp command. Only ever passes a browser name
    /// or a path — cookie values are never read by this app, so they cannot be
    /// logged by it either.
    pub fn apply(&self, cmd: &mut Command) {
        match self {
            CookieSource::None => {}
            CookieSource::Browser(b) => {
                if let Some(name) = b.ytdlp_name() {
                    cmd.arg("--cookies-from-browser").arg(name);
                }
            }
            CookieSource::File(path) => {
                cmd.arg("--cookies").arg(path);
            }
        }
    }

    /// How this mode reads in a log line. Never includes cookie data.
    pub fn describe(&self) -> String {
        match self {
            CookieSource::None => "no cookies (anonymous)".into(),
            CookieSource::Browser(b) => {
                format!("--cookies-from-browser {}", b.ytdlp_name().unwrap_or("none"))
            }
            CookieSource::File(path) => format!("--cookies {}", path.display()),
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, CookieSource::None)
    }
}

/// What a cookie probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieStatus {
    /// No browser selected.
    Disabled,
    /// Cookies were read successfully.
    Available { count: usize, source: String },
    /// The browser is not installed (no profile directory / cookie database).
    BrowserNotFound { browser: String },
    /// The database exists but its contents could not be decrypted.
    DecryptFailed { browser: String, app_bound: bool },
    /// Something else went wrong; carries yt-dlp's own wording.
    ProbeFailed { detail: String },
}

impl CookieStatus {
    pub fn ok(&self) -> bool {
        matches!(self, CookieStatus::Available { .. })
    }

    /// One-line status for the settings panel.
    pub fn headline(&self) -> String {
        match self {
            CookieStatus::Disabled => "Not using cookies".into(),
            CookieStatus::Available { count, .. } => {
                format!("✓ Cookies available ({count} cookies)")
            }
            CookieStatus::BrowserNotFound { browser } => {
                format!("✗ {browser} cookies unavailable. Browser not found.")
            }
            CookieStatus::DecryptFailed { .. } => "✗ Cannot access cookies".into(),
            CookieStatus::ProbeFailed { .. } => "✗ Cannot access cookies".into(),
        }
    }

    /// The actionable explanation shown under the status line.
    pub fn detail(&self) -> Option<String> {
        match self {
            CookieStatus::Disabled | CookieStatus::Available { .. } => None,
            CookieStatus::BrowserNotFound { browser } => Some(format!(
                "yt-dlp found no {browser} profile on this machine. Pick a browser you \
                 actually use, or select None to stay logged out."
            )),
            // Deliberately *not* the generic "close the browser and try again":
            // app-bound encryption is not a file lock, and closing the browser
            // changes nothing. Saying otherwise sends the user in circles.
            CookieStatus::DecryptFailed { browser, app_bound: true } => Some(format!(
                "{browser} seals its cookie database with app-bound encryption \
                 (Windows DPAPI), which yt-dlp cannot read — closing the browser will \
                 not help. Firefox is not affected and is the reliable choice here. \
                 Otherwise export cookies manually to a Netscape-format cookies.txt \
                 and set it below."
            )),
            CookieStatus::DecryptFailed { browser, app_bound: false } => Some(format!(
                "Unable to access {browser} cookies. Close the browser and try again, \
                 or export cookies manually to a Netscape-format cookies.txt and set \
                 it below."
            )),
            CookieStatus::ProbeFailed { detail } => Some(format!(
                "Unable to access browser cookies. Close the browser and try again, or \
                 export cookies manually.\n\nyt-dlp said: {detail}"
            )),
        }
    }
}

/// A URL that makes yt-dlp load the cookie jar and then stop.
///
/// Cookie extraction happens during extraction setup, so *some* URL is needed —
/// but this one resolves locally and fails instantly, so the probe never
/// touches the network or SoundCloud (which would risk a rate limit just to
/// render a settings panel).
const PROBE_URL: &str = "file:///soundcloud2mp4-cookie-probe";

/// Read the cookie jar to see whether it is usable, without any network access.
///
/// The probe always "fails" overall — `PROBE_URL` is not a real media file —
/// so the verdict comes from yt-dlp's cookie lines, not its exit code.
pub async fn probe(
    ytdlp: &str,
    browser: CookieBrowser,
    token: &CancellationToken,
    log_file: Option<&Path>,
) -> CookieStatus {
    let Some(name) = browser.ytdlp_name() else {
        return CookieStatus::Disabled;
    };

    let mut cmd = tool_command(ytdlp);
    cmd.args(["--cookies-from-browser", name, "--simulate", PROBE_URL]);

    let output = run_capture(cmd, ytdlp, token, log_file).await;
    let (_ok, stdout, stderr) = match output {
        Ok(v) => v,
        Err(e) => {
            return CookieStatus::ProbeFailed { detail: format!("{e:#}") };
        }
    };
    parse_probe(&format!("{stdout}\n{stderr}"), browser.label())
}

/// Classify yt-dlp's cookie chatter. Split out so the real message shapes can
/// be unit-tested without a browser or a yt-dlp binary.
pub fn parse_probe(output: &str, browser: &str) -> CookieStatus {
    let lower = output.to_lowercase();

    // "Extracted 2652 cookies from firefox"
    if let Some(count) = extract_count(output) {
        return CookieStatus::Available {
            count,
            source: browser.to_string(),
        };
    }
    // "could not find opera cookies database in ..."
    if lower.contains("could not find") && lower.contains("cookies database") {
        return CookieStatus::BrowserNotFound { browser: browser.to_string() };
    }
    // "Failed to decrypt with DPAPI. See .../10927"
    if lower.contains("failed to decrypt") || lower.contains("dpapi") {
        return CookieStatus::DecryptFailed {
            browser: browser.to_string(),
            app_bound: lower.contains("dpapi") || lower.contains("10927"),
        };
    }
    if lower.contains("permission denied") || lower.contains("database is locked") {
        return CookieStatus::DecryptFailed {
            browser: browser.to_string(),
            app_bound: false,
        };
    }

    let detail = output
        .lines()
        .map(str::trim)
        .find(|l| l.to_lowercase().starts_with("error:"))
        .unwrap_or("no cookies were extracted")
        .to_string();
    CookieStatus::ProbeFailed { detail }
}

/// Pull N out of "Extracted N cookies from <browser>".
fn extract_count(output: &str) -> Option<usize> {
    output.lines().find_map(|line| {
        let l = line.trim();
        let rest = l.strip_prefix("Extracted ")?;
        let (n, tail) = rest.split_once(' ')?;
        tail.starts_with("cookies").then(|| n.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_success_line() {
        let out = "[generic] Extracting URL: file:///x\nExtracting cookies from firefox\n\
                   Extracted 2652 cookies from firefox\nERROR: unable to download webpage";
        assert_eq!(
            parse_probe(out, "Firefox"),
            CookieStatus::Available { count: 2652, source: "Firefox".into() }
        );
        assert!(parse_probe(out, "Firefox").ok());
    }

    /// The Chromium-on-Windows case: Chrome, Edge and Brave all hit this, so
    /// the advice must not be "close the browser".
    #[test]
    fn app_bound_encryption_is_reported_as_such() {
        let out = "Extracting cookies from chrome\n\
                   ERROR: Failed to decrypt with DPAPI. See  \
                   https://github.com/yt-dlp/yt-dlp/issues/10927  for more info";
        let status = parse_probe(out, "Chrome");
        assert_eq!(
            status,
            CookieStatus::DecryptFailed { browser: "Chrome".into(), app_bound: true }
        );
        assert!(!status.ok());
        assert_eq!(status.headline(), "✗ Cannot access cookies");
        let detail = status.detail().unwrap();
        assert!(detail.contains("app-bound"), "{detail}");
        assert!(detail.contains("will not help"), "must not advise closing: {detail}");
        assert!(detail.contains("Firefox"), "should point at what does work: {detail}");
    }

    /// A plain lock/permission problem *is* worth closing the browser for, and
    /// gets the requested wording.
    #[test]
    fn a_locked_database_suggests_closing_the_browser() {
        let out = "Extracting cookies from chrome\nERROR: database is locked";
        let status = parse_probe(out, "Chrome");
        assert_eq!(
            status,
            CookieStatus::DecryptFailed { browser: "Chrome".into(), app_bound: false }
        );
        let detail = status.detail().unwrap();
        assert!(detail.contains("Close the browser and try again"), "{detail}");
        assert!(detail.contains("export cookies manually"), "{detail}");
    }

    #[test]
    fn a_missing_browser_is_named() {
        let out = "Extracting cookies from opera\n\
                   ERROR: could not find opera cookies database in \"C:\\...\\Opera Stable\"";
        let status = parse_probe(out, "Chrome");
        assert_eq!(status, CookieStatus::BrowserNotFound { browser: "Chrome".into() });
        assert_eq!(status.headline(), "✗ Chrome cookies unavailable. Browser not found.");
    }

    #[test]
    fn unrecognised_output_keeps_yt_dlps_own_wording() {
        let status = parse_probe("ERROR: something new and strange", "Edge");
        let CookieStatus::ProbeFailed { detail } = &status else {
            panic!("expected ProbeFailed, got {status:?}");
        };
        assert!(detail.contains("something new and strange"), "{detail}");
        assert!(status.detail().unwrap().contains("Close the browser"));
    }

    #[test]
    fn source_produces_the_right_flag_and_never_leaks_values() {
        let none = CookieSource::None;
        assert!(none.is_none());
        assert_eq!(none.describe(), "no cookies (anonymous)");

        let browser = CookieSource::Browser(CookieBrowser::Firefox);
        assert_eq!(browser.describe(), "--cookies-from-browser firefox");

        let file = CookieSource::File(PathBuf::from("c.txt"));
        assert_eq!(file.describe(), "--cookies c.txt");

        // Selecting "None" as a browser degrades to running logged out rather
        // than passing a bogus flag.
        assert_eq!(
            CookieSource::Browser(CookieBrowser::None).describe(),
            "--cookies-from-browser none"
        );
        let mut cmd = tool_command("yt-dlp");
        CookieSource::Browser(CookieBrowser::None).apply(&mut cmd);
        assert_eq!(cmd.as_std().get_args().count(), 0, "None must add no flag");
    }

    #[test]
    fn every_browser_maps_to_the_name_yt_dlp_expects() {
        assert_eq!(CookieBrowser::None.ytdlp_name(), None);
        assert_eq!(CookieBrowser::Chrome.ytdlp_name(), Some("chrome"));
        assert_eq!(CookieBrowser::Edge.ytdlp_name(), Some("edge"));
        assert_eq!(CookieBrowser::Firefox.ytdlp_name(), Some("firefox"));
    }
}
