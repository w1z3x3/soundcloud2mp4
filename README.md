# SoundCloud → MP4 Converter

Turn a SoundCloud playlist (or a single track) into MP4 music videos. Each track
becomes a video showing its cover artwork with the title and artist overlaid,
rendered at your chosen resolution and audio bitrate. You can export one file per
track, or stitch the entire playlist into a single long video with crossfade
transitions and per-track chapter markers.

It is a native desktop application written in Rust, built on **egui/eframe** for
the interface, **tokio** for background work, and the external tools **yt-dlp**
(downloading + metadata) and **ffmpeg** (audio conversion + video rendering).

> **Screenshots:**
> <img width="1114" height="832" alt="soundcloud" src="https://github.com/user-attachments/assets/976983b2-682c-4100-bcc2-b9d6deaad717" />


---

## Table of contents

- [Features](#features)
- [Supported platforms](#supported-platforms)
- [Installation](#installation)
- [Building from source](#building-from-source)
- [Usage](#usage)
- [Export modes](#export-modes)
- [Settings](#settings)
- [Resume workflow](#resume-workflow)
- [GPU acceleration](#gpu-acceleration)
- [Metadata recovery & retry](#metadata-recovery--retry)
- [SoundCloud authentication (cookies)](#soundcloud-authentication-cookies)
- [How it works](#how-it-works)
- [Configuration file](#configuration-file)
- [Performance](#performance)
- [Troubleshooting](#troubleshooting)
- [FAQ](#faq)
- [Limitations](#limitations)
- [Tests](#tests)
- [Project layout](#project-layout)
- [Legal](#legal)
- [License](#license)

---

## Features

- **Load any playlist or single track** — paste a SoundCloud URL and load its
  track list. The list shows a checkbox, thumbnail, title, uploader, duration and
  live status per track.
- **Two export modes** — one MP4 per track, or one long playlist video (see
  [Export modes](#export-modes)).
- **Per-track play time** — set how many seconds each song plays, and/or a global
  maximum per song (0 = full length).
- **Audio quality selector** — 64 / 128 / 192 / 256 / 320 kbps (encoded to AAC in
  the video; downloaded as MP3 at the chosen rate).
- **Resolution selector** — 1280×720, 1920×1080, 2560×1440, 3840×2160.
- **Optional effects** — fade in/out (video + audio) and a slow Ken-Burns zoom on
  the cover.
- **Combined playlist video** with configurable crossfade transitions, playlist
  metadata, and one chapter marker per track.
- **GPU-accelerated H.264 encoding** — NVIDIA NVENC, Intel Quick Sync, or AMD AMF,
  with automatic detection and a safe fallback to CPU (`libx264`). See
  [GPU acceleration](#gpu-acceleration).
- **Crash-resumable combined exports** — an interrupted export continues from the
  clips and batches it already produced instead of starting over. See
  [Resume workflow](#resume-workflow).
- **Persistent metadata cache** — a playlist you have loaded before reopens
  almost instantly, and only missing tracks are re-fetched.
- **Automatic metadata recovery** — the app keeps re-fetching tracks that failed
  to a rate limit in the background, so you don't have to keep clicking retry.
- **Scales to very large playlists** — a two-tier combine ladder keeps memory and
  the command line bounded even at hundreds of tracks.
- **Browser cookie authentication** — point yt-dlp at a browser you're already
  signed in to, for tracks that require a session.
- **Built-in encoder benchmark** — render a 30-second sample with any encoder to
  compare speed before committing to a long job.
- **Guided tool setup** — on Windows the app can install ffmpeg and yt-dlp for you
  via `winget`.
- **Non-blocking UI** — all work runs on a background async runtime with live
  progress, phase text, logs and cancellation.
- **Settings persisted** automatically to `config.json`.

---

## Supported platforms

The application builds and runs on **Windows, macOS and Linux**. Some conveniences
are platform-specific:

| Capability | Windows | macOS | Linux |
| --- | :---: | :---: | :---: |
| Core download / render / combine | ✅ | ✅ | ✅ |
| GPU encoder detection (device query) | ✅ (WMI / PowerShell) | functional probe only¹ | functional probe only¹ |
| Automatic tool install (`winget`) | ✅ | manual (Homebrew) | manual (package manager) |
| Executable icon embedding | ✅ | — | — |

¹ On non-Windows systems the GPU vendor list is not queried, so encoder
availability is decided purely by a real test-encode (see
[GPU acceleration](#gpu-acceleration)). The result is the same: only encoders that
actually work are offered.

---

## Installation

The app needs two external tools on your system: **yt-dlp** and **ffmpeg** (which
also provides **ffprobe**). Either put them on your `PATH` or set their full paths
in the app under **Settings → Tool paths**. The header shows a green ✓ or red ✗ for
each, and a conversion cannot start until both are found.

**Windows**

```bash
winget install yt-dlp.yt-dlp
winget install Gyan.FFmpeg
```

On Windows, if either tool is missing the app offers a one-click setup that runs
the two `winget` commands for you, refreshes `PATH`, and re-checks — including a
UAC elevation prompt if the install needs it.

**macOS**

```bash
brew install yt-dlp ffmpeg
```

**Linux**

```bash
sudo apt install yt-dlp ffmpeg     # or your distribution's equivalent / pipx install yt-dlp
```

Keep yt-dlp reasonably current (`yt-dlp -U`) — SoundCloud extraction changes over
time, and some tracks only resolve on newer versions. The **Tool paths** panel
reports the version in use.

---

## Building from source

Requires a recent stable **Rust** toolchain (install from <https://rustup.rs>).

```bash
cargo build --release
```

The binary is produced at `target/release/soundcloud2mp4` (`.exe` on Windows).

For development:

```bash
cargo run
```

---

## Usage

1. Paste a SoundCloud playlist URL, e.g. `https://soundcloud.com/xxxxxxxxxxxxxxxx`.
2. Click **Load Playlist** (or press Enter). Rows appear immediately and fill in
   their metadata as it loads.
3. Untick tracks you don't want; adjust each track's play time in its row, if
   desired.
4. Choose audio quality, resolution, output folder, an optional global max per
   song, and effects.
5. Pick an [export mode](#export-modes) and, for GPU users, an
   [encoder](#gpu-acceleration).
6. Click **▶ Convert**. Watch progress, phase text and the log panel (**Show
   logs**); press **⏹ Cancel** at any time.
7. Output files appear in your chosen folder. In separate mode each track is
   `Artist - Title.mp4`; in combined mode the single file is named after the
   playlist (or a name you set).

---

## Export modes

Selected under **Settings → Export**.

### Separate videos

One `Artist - Title.mp4` per selected track. Each video loops the cover image for
the chosen play length, overlays the title and artist, applies your fade/zoom
effects, and embeds `title` / `artist` metadata. Tracks are processed
sequentially; if one fails the others continue (unless you turn off *Continue when
a track fails*).

### One long playlist video

Every selected track is rendered to a fixed-length intermediate clip, then all
clips are stitched into a single MP4:

- **Transitions** — with a transition length above 0, tracks are joined with a
  video crossfade (`xfade`) and matching audio crossfade (`acrossfade`). The
  transition is automatically clamped so it can never exceed the shortest clip.
  With a transition of 0, tracks are joined as hard cuts.
- **Chapters** — one chapter marker per track (title = `Artist - Title`) is
  embedded, so players can jump between songs. Toggle with *Add chapter markers*.
- **Accurate timing** — offsets, chapter marks and totals are computed from the
  *measured* duration of each rendered clip (via ffprobe), not the requested
  length, so long playlists stay in sync and chapters land on the right track.
- **Re-encoded, not concatenated** — the final video is always re-encoded so
  transitions, timing and metadata are correct.

The combined export is **resumable** and uses a **two-tier combine ladder** for
large playlists — see [Resume workflow](#resume-workflow) and
[How it works](#how-it-works).

---

## Settings

All settings are in the app and are saved to `config.json` automatically.

| Setting | Meaning | Default |
| --- | --- | --- |
| **Audio quality** | Bitrate in kbps (64/128/192/256/320). | `320` |
| **Resolution** | Output resolution. | `1920x1080` |
| **Output folder** | Where finished videos are written. | *system Videos folder* `/SoundCloud Videos` |
| **Max per song** | Global cap on how long any track plays (0 = full length). | `0` |
| **Fade** | Fade in/out on video and audio. | on |
| **Zoom** | Slow Ken-Burns zoom on the cover. | off |
| **Export mode** | Separate videos or one playlist video. | Separate |
| **Playlist video name** | Output name for the combined file (empty = playlist title). | *(empty)* |
| **Transition length** | Crossfade seconds between tracks (combined mode). | `2.0` |
| **Add chapter markers** | Embed per-track chapters in the combined file. | on |
| **Continue when a track fails** | Keep going if a track fails, instead of aborting. | on |
| **Encoder** | Auto / CPU / NVENC / QSV / AMF. | Auto |
| **Hardware decode** (experimental) | Offload input *decoding* to the GPU during combine. | off |
| **Combine batch size** | Clips per first-level combine pass (16–64). | `40` |
| **Batch-combine size** | Intermediate batches per upper-level pass (2–16). | `4` |
| **Automatically retry transient failures** | Resilient inline retry with a shared cooldown during load + retry pass. | on |
| **Automatically refresh browser cookies** | One cookie refresh + retry on auth/restricted errors. | on |
| **Max attempts** | Fetch attempts per track, including the first (1–10). | `6` |
| **Initial delay** | First rung of the exponential back-off (s). | `5` |
| **Max delay** | Cap the doubling back-off (s). | `60` |
| **Auto-recover metadata** | Keep retrying rate-limited tracks in the background. | on |
| **Retry-pass delay** | Extra pause before each request in the retry pass (ms). | `500` |
| **Parallel retries** | Retry-pass workers (1–4, never above the load's concurrency). | `2` |
| **yt-dlp path / ffmpeg path** | Override if the tools aren't on `PATH`. | `yt-dlp` / `ffmpeg` |
| **SoundCloud authentication** | Browser to read cookies from (or a cookies file fallback). | None |
| **Debug mode** | Also write `app.log`, `yt-dlp.log`, `ffmpeg.log`. | off |

Out-of-range values in a hand-edited `config.json` are clamped on load, so the
file can't push the retry pass or combine ladder into unsafe territory.

---

## Resume workflow

**Combined exports are crash-resumable.** All expensive intermediate work lives in
a `.work` folder inside the output directory, and that folder is **never deleted
automatically except after a fully successful final render** (or when you choose
*Start over*). So a crash, cancellation or power loss mid-export loses almost
nothing.

How resume decides what to reuse:

- **Rendered clips** (`clip_XXXX.mp4`) are reused if the file exists *and* ffprobe
  can read a valid duration from it. A clip a crash truncated is treated as missing
  and rebuilt. Reuse skips the download and render entirely.
- **Combine batches** are reused only if they exist, probe cleanly, *and* their
  measured length matches what the current plan expects — so a batch left over from
  a different clip set or transition length is rebuilt rather than trusted.
- A lightweight checkpoint (`.work/resume.json`) records the stage, finished clips,
  finished batches and the encoder in use, so the app can state where it's resuming
  from. The checkpoint is **advisory only** — every artifact it names is still
  ffprobe-validated before reuse, so resume works correctly even with no checkpoint
  at all.

In the GUI, if you press **Convert** in combined mode while a `.work` folder holds
reusable artifacts, a **Resume previous conversion?** dialog appears:

- **Resume previous conversion** (recommended) — continue from existing clips and
  batches.
- **Start over (delete work directory)** — wipe `.work` and begin fresh.

> **Tip:** before resuming a large playlist, let metadata finish loading (use
> **Retry Failed Metadata** if any tracks are still missing). Chapter titles for
> reused clips come from the loaded track metadata, so a track whose metadata
> didn't reload would get a placeholder chapter name even though its clip is reused
> fine.

The **persistent metadata cache** is separate from `.work` and survives it: one
JSON file per playlist under the app's `metadata/` directory. Reopening a
previously loaded playlist hydrates the list instantly and only re-fetches tracks
that are still incomplete.

---

## GPU acceleration

The app can encode the H.264 video on your GPU. Compositing (scaling, text overlay,
crossfades) always runs on the CPU; only the H.264 encode — the actual bottleneck
for a static image with text — is offloaded.

**Encoder options** (**Settings → Encoding**):

| Choice | Uses | Notes |
| --- | --- | --- |
| **Auto** (recommended) | Best available hardware encoder, else CPU | Priority NVENC → QSV → AMF → CPU |
| **CPU (libx264)** | Software | Always available; the universal fallback |
| **NVIDIA NVENC** | `h264_nvenc` | Requires an NVIDIA GPU |
| **Intel Quick Sync** | `h264_qsv` | Requires Intel Quick Sync graphics |
| **AMD AMF** | `h264_amf` | Requires an AMD GPU |

**Detection is functional, not a feature-list lookup.** Common ffmpeg builds ship
NVENC, QSV *and* AMF compiled in on every machine, so `ffmpeg -encoders` cannot tell
you what your hardware can actually run. Availability is therefore decided in three
steps, and only the last counts as a yes:

1. Is the encoder compiled into your ffmpeg at all?
2. Is a GPU of the matching vendor present? (Windows device query; skipped
   elsewhere.)
3. Does a tiny real encode to `-f null` actually succeed?

Anything short of a clean step 3 is reported as unavailable **with the reason**
(shown as a tooltip on the disabled option), and the app falls back to CPU. If you
pick a specific encoder that isn't usable, it transparently falls back to CPU with a
note. The last detection result is cached (`encoders.json`) so the UI shows encoder
availability instantly on the next launch while a fresh probe refreshes it in the
background.

**Hardware decode** (experimental, off by default) additionally offloads input
*decoding* during the combine stage (`-hwaccel`, e.g. `cuda` / `qsv` / `d3d11va`).
GPU *encoding* alone already provides most of the speed-up, and hardware decode can
be memory-heavy with complex filter graphs — so it's opt-in. If a combine pass
fails in a way that looks like GPU memory exhaustion, the app automatically retries
that pass once with software decoding before giving up.

**Benchmark.** The Encoding panel has a **⏱ Benchmark (30s sample)** button that
renders a synthetic 30-second sample with the selected encoder and reports elapsed
time, average FPS and file size. When a playlist is loaded it also projects an
estimated full-render time per encoder, so you can compare CPU vs GPU before
starting a long job.

---

## Metadata recovery & retry

SoundCloud's flat playlist listing returns only an id and URL per track, so the app
fetches full metadata (title, uploader, duration, artwork) per track in a second
pass. On large playlists this pass can hit SoundCloud's rate limiting. The app
treats metadata loading like a **resilient downloader** so a brief rate limit never
strands part of a playlist:

- **Inline resilient retry** (on by default) — during the initial load *and* the
  retry pass, a transient failure (HTTP 429, 5xx, timeout, connection reset, TLS/DNS
  trouble) does **not** mark the track failed. Instead the whole batch pauses on a
  single **global cooldown** and retries with **exponential back-off** (default
  5s → 10s → 20s → 40s → 60s → 60s, one rung per attempt). Because the cooldown is
  shared, one 429 pauses every worker at once rather than each worker independently
  hammering and extending the limit.
- **One-shot cookie refresh** (on by default) — a failure that looks like it needs
  authentication (DRM/restricted, HTTP 401, login required, expired cookies) triggers
  a single refresh of the selected browser's cookies followed by one retry, before
  the track is reported. No effect when no browser is selected.
- **Automatic background recovery** (on by default) — a second line of defence: if a
  hard rate limit persists past the inline retries, the app keeps re-running the
  retry pass in the background on a widening delay until every recoverable track
  resolves or you press **Stop**. No repeated button-clicking required.
- **Manual retry** — a **⟳ Retry Failed Metadata (N)** button above the track list
  re-runs the pass on demand.

Every failure is routed through one central classifier into a single decision:

- **Retry** — rate limiting, 5xx, timeouts, connection resets, broken pipes, network
  unreachable, TLS handshake, DNS lookup and proxy errors, plus anything
  unrecognised. (SoundCloud reports throttling as HTTP `403`, never `429`, so 403 is
  treated as transient.)
- **Refresh cookies and retry** — DRM/restricted, 401, login-required or
  expired-cookie errors. Refreshed once and retried, then reported.
- **Permanent** — deleted, private, region-locked, unsupported/invalid URL, 404/410
  or malformed responses. Never retried and auto-deselected.

The retry pass is deliberately gentler than the initial load (fewer workers and an
extra delay before each request) so it can't hammer harder than the load that
already failed. All retry behaviour is configurable (see [Settings](#settings)) and
every decision — cooldowns, refreshes, attempts, give-ups — is written to the live
log panel.

---

## SoundCloud authentication (cookies)

Some tracks' plain MP3 streams are served only to a signed-in session. Rather than
asking you to export a `cookies.txt` (browsers increasingly export JSON, which
yt-dlp rejects), the app can point yt-dlp straight at a browser you're already
signed in to.

Under **Settings → SoundCloud Authentication**, pick a browser (None / Chrome / Edge
/ Firefox). The selection becomes `--cookies-from-browser <name>` on every yt-dlp
call. A **Check** button runs an offline probe that reads the cookie jar and stops,
so the status shown is real rather than assumed. The app never reads cookie values
itself — it only ever passes yt-dlp a browser name — and only the cookie *mode* is
ever logged.

> **Chromium on Windows usually cannot be read.** Since Chrome 127, Chromium-based
> browsers (Chrome, Edge, Brave) seal their cookie database with app-bound
> encryption that yt-dlp cannot decrypt (`Failed to decrypt with DPAPI`). Firefox
> reads fine. The status line says so plainly and points you at Firefox. Closing
> the browser does not help.

A Netscape `cookies.txt` file field is available as a fallback, but it stays hidden
until a browser probe actually fails. When set and valid it takes precedence; when
set but missing or in the wrong format it's ignored with a warning (a bad
`--cookies` path would otherwise break every request).

---

## How it works

Playlist loading is two-stage, because SoundCloud's `--flat-playlist` entries carry
only an id and URL:

1. `yt-dlp --flat-playlist -J <playlist>` — the track list appears instantly.
2. Per track, `yt-dlp --dump-single-json --no-download` (several in parallel) fills
   in title / uploader / duration / thumbnail, with sensible fallbacks. Missing
   fields are reported on the row rather than silently replaced.

Rendering, per track:

```
yt-dlp   — --extract-audio --audio-format mp3 --audio-quality <Q>K
           --write-thumbnail --write-info-json  →  track.mp3 / track.jpg / track.info.json
validate — confirm audio / info.json / thumbnail exist; re-resolve metadata
ffmpeg   — cover image + audio → MP4. Run with cwd = the track's work dir so the
           filtergraph references only relative files (font / title / artist text),
           scaled and padded to the target resolution, title + artist drawn on,
           encoded with the selected encoder + AAC audio at <Q> kbps.
```

Tracks with no artwork get an auto-generated gradient placeholder cover, so a render
never fails on missing artwork.

**Combined mode — scaling to hundreds of tracks.** The combine stage runs as a
*ladder of ffmpeg passes*, not one giant command, because a single command at ~500
tracks would blow past the Windows command-line length limit. Two caps bound the
ladder:

- **First level (clips → batches):** up to *Combine batch size* clips per pass
  (default 40).
- **Upper levels (batches → batches → final):** up to *Batch-combine size*
  intermediates per pass (default 4). Batch inputs are full-motion re-encoded
  videos, far heavier to decode than a still-image clip, so only a few are opened at
  once — this is what keeps peak memory bounded on very large playlists.

The filtergraph is written to a script file (`-filter_complex_script`) rather than
the command line, and all paths are relative to the work directory. Batching is
mathematically equivalent to a single command (crossfade is associative across batch
boundaries and durations compose), so timing and chapters are identical regardless
of how many levels the ladder needs. Intermediate files are deleted as soon as the
pass consuming them succeeds, so disk use never balloons.

---

## Configuration file

Settings are saved automatically to:

- **Windows:** `%APPDATA%\soundcloud2mp4\config.json`
- **Linux:** `~/.config/soundcloud2mp4/config.json`
- **macOS:** `~/Library/Application Support/soundcloud2mp4/config.json`

The same directory also holds `encoders.json` (cached encoder detection), a
`metadata/` folder (per-playlist metadata caches), a `debug/` folder (raw yt-dlp
JSON), and — when debug mode is on — a `logs/` folder.

A minimal example is in [config.example.json](config.example.json). Unknown or
omitted fields fall back to defaults, so the file can be partially specified:

```json
{
  "default_quality": "320",
  "default_resolution": "1920x1080",
  "output_folder": "Videos/SoundCloud Videos",
  "max_track_seconds": 0.0,
  "effect_zoom": false,
  "effect_fade": true,
  "ytdlp_path": "yt-dlp",
  "ffmpeg_path": "ffmpeg",
  "export_mode": "Separate",
  "transition_seconds": 2.0,
  "enable_chapters": true,
  "continue_on_fail": true
}
```

---

## Performance

Encoding speed depends mainly on your chosen encoder (a GPU encoder is dramatically
faster than CPU `libx264` for this static-image-plus-text workload), the output
resolution, and total play time. Use the built-in **Benchmark** button to measure
your own hardware and compare encoders before starting a long job — see
[GPU acceleration](#gpu-acceleration).

> **Note:** the application does not record total encode time. Timing is shown live
> during a run but is not persisted anywhere, and the work directory is deleted on
> success — so after the fact there is no log from which a full encode duration
> could be recovered. Enable **Debug mode** before a run if you want per-command
> timing captured to `logs/`.

The two-tier combine ladder and resume system are designed for large playlists
(hundreds of tracks) producing a single chaptered MP4, keeping peak memory bounded
regardless of playlist size.

---

## Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| Red ✗ next to ffmpeg or yt-dlp in the header | The tool isn't on `PATH`. Install it, or set its full path under **Settings → Tool paths** (Windows users can use the built-in installer). |
| A specific GPU encoder is greyed out | Detection's real test-encode failed. Hover the option for the reason; the app uses CPU instead. Update GPU drivers / ffmpeg if you expected it to work. |
| Combined export failed on the final pass with a memory error | GPU decode memory exhaustion. The app auto-retries with software decode; if it persists, turn off **Hardware decode** and/or lower **Batch-combine size**. |
| Many tracks stuck "fetching metadata…" on a big playlist | SoundCloud rate limiting. Leave auto-recovery running, or press **Retry Failed Metadata**. This is expected on large playlists and recovers over time. |
| A track shows "DRM protected" / restricted | SoundCloud withheld the stream from an anonymous client. Sign in to SoundCloud in Firefox, select it under **SoundCloud Authentication**, and retry. |
| Chrome/Edge cookies show "Cannot access cookies" | App-bound encryption on Windows; yt-dlp can't decrypt it. Use **Firefox** instead. |
| Reused-clip chapters show placeholder names after a resume | Metadata for those tracks hadn't reloaded. Let metadata finish (Retry Failed Metadata) before converting. |
| Export was interrupted | Just press **Convert** again and choose **Resume** — finished clips and batches are reused. |
| Want to see exactly what ran | Enable **Debug mode** for `logs/`, or open **Show logs** in-app. Raw yt-dlp JSON is always saved under the `debug/` folder. |

---

## FAQ

**Can I convert a single track instead of a playlist?**
Yes — paste a track URL. It loads as a one-row list.

**Where do the finished files go?**
Your configured output folder. Separate mode writes `Artist - Title.mp4` per track;
combined mode writes one file named after the playlist (or the name you set).

**Does it re-download everything if I restart a combined export?**
No. Rendered clips and combine batches are reused after validation. See
[Resume workflow](#resume-workflow).

**Do I have to sign in?**
No. Most tracks work anonymously. Cookies only help for tracks SoundCloud withholds
from anonymous clients.

**Which encoder should I choose?**
Leave it on **Auto** — it picks the best working GPU encoder and falls back to CPU
if none is usable.

**Why is metadata loading so slow on huge playlists?**
SoundCloud rate-limits per-track metadata requests. The app deliberately paces
itself and recovers the tail in the background rather than getting the whole
playlist blocked.

**Can I change how aggressively it retries?**
Yes — the retry delay, worker count and back-off ladder are all adjustable under
**Settings → Metadata retry**.

---

## Limitations

- Requires external `yt-dlp` and `ffmpeg`/`ffprobe`; they are not bundled.
- SoundCloud extraction depends on yt-dlp; some tracks require a current yt-dlp
  and/or a signed-in browser session, and truly removed/private tracks can't be
  recovered.
- GPU vendor detection queries the OS only on Windows; elsewhere availability is
  decided by the functional test-encode alone.
- The automatic tool installer uses `winget` and is Windows-only.
- Output is always H.264 video with AAC audio in an MP4 container.
- Combined exports re-encode the whole playlist (required for correct transitions,
  timing and metadata), which takes proportionally longer than separate exports.
- Total encode time is not logged (see [Performance](#performance)).

---

## Tests

```bash
cargo test                                          # unit + fixture tests (offline)
cargo test --test e2e_workflow -- --ignored         # separate mode: real playlist → per-track MP4s
cargo test --test combined_workflow -- --ignored    # combined mode: real playlist → one MP4 + chapters
cargo test --test large_combine -- --ignored        # combine ladder: synthetic clips, batched (no network)
cargo test --test metadata_retry -- --ignored       # retry/recovery pass behaviour
```

The `--ignored` tests reach the network and/or run ffmpeg; the default `cargo test`
run is fully offline. Fixtures in `tests/fixtures/` are captured yt-dlp responses
covering JSON parsing, metadata fallback chains, missing-file validation and ffmpeg
command generation.

---

## Project layout

```
src/
├── main.rs              — entry point, logging, window setup
├── setup.rs             — optional ffmpeg/yt-dlp installer (winget, Windows)
├── pipeline.rs          — background orchestration: load, convert, benchmark, retry
├── gui/
│   ├── app.rs           — eframe App: state, message pump, all panels & dialogs
│   └── components.rs     — track-row widget, status badges
├── downloader/
│   ├── ytdlp.rs         — playlist fetch, per-track metadata, download, failure classification
│   ├── metadata.rs      — info.json parsing, locating audio/cover files
│   └── cookies.rs       — browser-cookie authentication + probing
├── video/
│   ├── encoder.rs       — encoder selection, functional GPU detection, ffmpeg args
│   ├── ffmpeg.rs        — per-track command/filter construction, font discovery
│   ├── renderer.rs      — per-track render flow (shared by both export modes)
│   ├── concat.rs        — PlaylistRenderer: two-tier combine ladder, transitions, chapters
│   ├── probe.rs         — ffprobe duration measurement
│   └── checkpoint.rs    — resume.json progress checkpoint
├── models/
│   ├── track.rs         — Track, TrackStatus, TrackMetadata
│   ├── cache.rs         — persistent per-playlist metadata cache
│   └── messages.rs      — worker → GUI message types
├── config/
│   └── settings.rs      — Settings load/save (serde JSON)
└── utils/
    ├── filesystem.rs    — filename sanitizing, unique names, duration formatting
    └── process.rs       — cancellable child processes with live log streaming
```

---

## Legal

Only download content you have the rights to — your own uploads, Creative Commons
tracks, or tracks whose owners permit downloading. Respect SoundCloud's Terms of
Service.

---

## License

[MIT](https://github.com/w1z3x3/soundcloud2mp4?tab=MIT-1-ov-file)
