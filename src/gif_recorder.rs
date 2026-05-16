//! Screen-region GIF recorder for Wayland (Hyprland-friendly).
//!
//! Pipeline:
//!
//!   slurp           — interactive region select
//!     │
//!     ▼  "X,Y WxH"
//!   wf-recorder     — wlr-screencopy → libx264 MP4 (graceful on SIGINT)
//!     │
//!     ▼  /tmp/jot-…-source.mp4
//!   ffmpeg palettegen → palette.png
//!     │
//!     ▼
//!   ffmpeg paletteuse with Sierra2_4a dithering → final.gif
//!     │
//!     ▼
//!   ~/Videos/gif/jot-YYYY-MM-DD-HHMMSS.gif
//!
//! Lifetime and safety:
//!
//! * Every subprocess is wrapped in `ChildGuard`, which on `Drop` sends
//!   SIGINT, waits up to 3 s, then SIGKILL. So no matter how this
//!   function exits — early return, panic, channel close — we never
//!   leave stray `wf-recorder` or `ffmpeg` processes pinning the
//!   compositor or holding the screen-record protocol.
//! * The intermediate MP4 lives in `$XDG_CACHE_HOME/jot/recordings/`
//!   and is deleted on success OR on cancel (best-effort).
//! * All required external tools (`slurp`, `wf-recorder`, `ffmpeg`) are
//!   probed with `which` before any subprocess is started. Missing
//!   tools surface as a friendly `RecorderEvent::Error` — never a panic.
//! * The worker runs on its own OS thread; only `async_channel` carries
//!   events back to the GLib main loop. Nothing in this file touches
//!   GTK directly.

use anyhow::{anyhow, bail, Context, Result};
use async_channel::{Receiver, Sender};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TICK_INTERVAL: Duration = Duration::from_millis(200);
const GRACEFUL_KILL_TIMEOUT: Duration = Duration::from_secs(3);

/// Commands the UI sends to the worker.
#[derive(Debug, Clone, Copy)]
pub enum RecorderCmd {
    /// Finish the recording and start GIF conversion.
    Stop,
    /// Abort whatever phase we're in. Cleans up partial files.
    Cancel,
}

/// Events the worker emits. All consumed via `glib::spawn_future_local`.
#[derive(Debug, Clone)]
pub enum RecorderEvent {
    /// `slurp` is up — the editor should hide itself if open.
    SelectingRegion,
    /// Region picked, `wf-recorder` is running. UI can show "● Recording".
    /// UI computes the overlay anchor from this region.
    RecordingStarted { region: String },
    /// One per second while recording. `seconds` counts from start.
    Tick { seconds: u64 },
    /// Stop received, ffmpeg pipeline starting.
    Converting,
    /// Final GIF is on disk.
    Done { gif_path: PathBuf, seconds: u64 },
    /// User cancelled (Esc in slurp, or pressed Cancel mid-flow).
    Cancelled,
    /// Anything else went wrong. Human-readable string for a toast.
    Error(String),
}

pub struct RecorderHandle {
    pub cmd_tx: Sender<RecorderCmd>,
}

/// Spin up the recorder worker. Returns a command sender and an event
/// receiver. Dropping `RecorderHandle` is fine — the receiver also being
/// dropped will cause the worker to clean up and exit.
pub fn start(
    fps: u32,
    quality: crate::config::GifQuality,
) -> (RecorderHandle, Receiver<RecorderEvent>) {
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<RecorderCmd>();
    let (evt_tx, evt_rx) = async_channel::unbounded::<RecorderEvent>();
    let evt_tx_for_thread = evt_tx.clone();

    std::thread::Builder::new()
        .name("jot-recorder".into())
        .spawn(move || {
            if let Err(e) = run_session(fps, quality, evt_tx_for_thread.clone(), cmd_rx) {
                tracing::error!("recorder session failed: {e:#}");
                let _ = evt_tx_for_thread.send_blocking(RecorderEvent::Error(format!("{e:#}")));
            }
        })
        .expect("spawn jot-recorder thread");

    (RecorderHandle { cmd_tx }, evt_rx)
}

// ─────────────────────────── Session loop ────────────────────────────

fn run_session(
    fps: u32,
    quality: crate::config::GifQuality,
    evt_tx: Sender<RecorderEvent>,
    cmd_rx: Receiver<RecorderCmd>,
) -> Result<()> {
    require_tool("slurp")?;
    require_tool("wf-recorder")?;
    require_tool("ffmpeg")?;

    // ── 1. region select ──────────────────────────────────────────────
    let _ = evt_tx.send_blocking(RecorderEvent::SelectingRegion);
    let region = match run_slurp() {
        Ok(Some(r)) => r,
        Ok(None) => {
            let _ = evt_tx.send_blocking(RecorderEvent::Cancelled);
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    // ── 2. wf-recorder ────────────────────────────────────────────────
    let cache_dir = ensure_cache_dir()?;
    let timestamp = file_timestamp();
    let mp4_path = cache_dir.join(format!("jot-{timestamp}-source.mp4"));

    let mut rec = ChildGuard::new(
        Command::new("wf-recorder")
            .args([
                "-g",
                &region,
                "-f",
                mp4_path.to_str().context("mp4 path is not utf-8")?,
                "-r",
                &fps.to_string(),
                "-c",
                "libx264",
                "-p",
                "preset=ultrafast",
                "-p",
                &format!("crf={}", quality_crf(quality)),
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn wf-recorder")?,
        "wf-recorder",
    );
    let _ = evt_tx.send_blocking(RecorderEvent::RecordingStarted {
        region: region.clone(),
    });

    // Wait for Stop / Cancel, ticking each second. Also notice if
    // wf-recorder dies on its own (e.g. compositor disconnect).
    let start = Instant::now();
    let mut last_tick = 0u64;
    let stop_requested = loop {
        if let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                RecorderCmd::Stop => break true,
                RecorderCmd::Cancel => {
                    rec.kill_now()?;
                    let _ = std::fs::remove_file(&mp4_path);
                    let _ = evt_tx.send_blocking(RecorderEvent::Cancelled);
                    return Ok(());
                }
            }
        }
        if let Some(status) = rec.try_wait()? {
            return Err(anyhow!("wf-recorder exited prematurely (status {status})"));
        }
        let elapsed = start.elapsed().as_secs();
        if elapsed > last_tick {
            last_tick = elapsed;
            let _ = evt_tx.send_blocking(RecorderEvent::Tick { seconds: elapsed });
        }
        std::thread::sleep(TICK_INTERVAL);
    };
    if !stop_requested {
        // unreachable today (loop break is always true), but keep the
        // shape so future cancellation branches don't fall through here.
        return Ok(());
    }

    // ── 3. flush wf-recorder via SIGINT ───────────────────────────────
    rec.interrupt_then_wait().context("stopping wf-recorder")?;
    let recording_secs = start.elapsed().as_secs().max(1);

    // ── 4. convert to GIF ─────────────────────────────────────────────
    let _ = evt_tx.send_blocking(RecorderEvent::Converting);
    let gif_dir = ensure_gif_output_dir()?;
    let gif_path = gif_dir.join(format!("jot-{timestamp}.gif"));
    let palette = cache_dir.join(format!("jot-{timestamp}-palette.png"));

    let convert_result = convert_to_gif(
        &mp4_path, &palette, &gif_path, fps, quality, &cmd_rx, &evt_tx,
    );

    // Always remove intermediates, even on failure.
    let _ = std::fs::remove_file(&mp4_path);
    let _ = std::fs::remove_file(&palette);

    match convert_result {
        Ok(ConvertOutcome::Done) => {
            let _ = evt_tx.send_blocking(RecorderEvent::Done {
                gif_path,
                seconds: recording_secs,
            });
            Ok(())
        }
        Ok(ConvertOutcome::Cancelled) => {
            let _ = std::fs::remove_file(&gif_path);
            let _ = evt_tx.send_blocking(RecorderEvent::Cancelled);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&gif_path);
            Err(e)
        }
    }
}

// ───────────────────────────── slurp ─────────────────────────────────

/// Run `slurp` and return `Some(region)` if the user picked one, or
/// `None` if they pressed Esc (slurp exits non-zero with empty stdout).
fn run_slurp() -> Result<Option<String>> {
    let out = Command::new("slurp")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawning slurp")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() || stdout.is_empty() {
        tracing::info!(
            "slurp cancelled or failed (status {:?}): {stderr}",
            out.status
        );
        return Ok(None);
    }
    Ok(Some(stdout))
}

// ────────────────────────── ffmpeg pipeline ──────────────────────────

enum ConvertOutcome {
    Done,
    Cancelled,
}

/// Two-pass palette pipeline. Identical to scripts/record-gif.sh: yields
/// sharp text and clean gradients in a small GIF.
fn convert_to_gif(
    mp4: &Path,
    palette: &Path,
    gif: &Path,
    fps: u32,
    quality: crate::config::GifQuality,
    cmd_rx: &Receiver<RecorderCmd>,
    evt_tx: &Sender<RecorderEvent>,
) -> Result<ConvertOutcome> {
    let palette_stats = quality_palette_stats(quality);
    let dither = quality_dither(quality);

    // Pass 1 — generate palette.
    let mut pass1 = ChildGuard::new(
        Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(mp4)
            .args([
                "-vf",
                &format!(
                    "fps={fps},colorspace=all=bt709:iall=bt709:range=pc:irange=tv,format=rgb24,palettegen=stats_mode={palette_stats}"
                ),
            ])
            .arg(palette)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg palettegen")?,
        "ffmpeg palettegen",
    );
    if let Some(out) = wait_with_cancel(&mut pass1, cmd_rx)? {
        if !out.status.success() {
            bail!("ffmpeg palettegen failed: {}", out.stderr);
        }
    } else {
        let _ = evt_tx.send_blocking(RecorderEvent::Cancelled);
        return Ok(ConvertOutcome::Cancelled);
    }

    // Pass 2 — apply palette with Sierra2_4a dithering.
    let mut pass2 = ChildGuard::new(
        Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-i",
            ])
            .arg(mp4)
            .arg("-i")
            .arg(palette)
            .args([
                "-lavfi",
                &format!(
                    "fps={fps},colorspace=all=bt709:iall=bt709:range=pc:irange=tv,format=rgb24[v];[v][1:v]paletteuse=dither={dither}:diff_mode=rectangle"
                ),
            ])
            .arg(gif)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg paletteuse")?,
        "ffmpeg paletteuse",
    );
    if let Some(out) = wait_with_cancel(&mut pass2, cmd_rx)? {
        if !out.status.success() {
            bail!("ffmpeg paletteuse failed: {}", out.stderr);
        }
    } else {
        let _ = evt_tx.send_blocking(RecorderEvent::Cancelled);
        return Ok(ConvertOutcome::Cancelled);
    }

    Ok(ConvertOutcome::Done)
}

struct CapturedOutput {
    status: std::process::ExitStatus,
    stderr: String,
}

/// Poll the child until it exits, while also listening for a Cancel
/// command. Returns `None` if cancelled, `Some(output)` otherwise.
/// Stderr is captured (best-effort) for error reporting.
fn wait_with_cancel(
    child: &mut ChildGuard,
    cmd_rx: &Receiver<RecorderCmd>,
) -> Result<Option<CapturedOutput>> {
    loop {
        if let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, RecorderCmd::Cancel) {
                child.kill_now()?;
                return Ok(None);
            }
        }
        if let Some(status) = child.try_wait()? {
            let stderr = child.drain_stderr();
            return Ok(Some(CapturedOutput { status, stderr }));
        }
        std::thread::sleep(TICK_INTERVAL);
    }
}

// ────────────────────────── ChildGuard ───────────────────────────────

/// RAII wrapper that guarantees no subprocess survives the worker.
///
/// On Drop: SIGINT → wait up to GRACEFUL_KILL_TIMEOUT → SIGKILL. The
/// `wf-recorder` graceful path is what gives us a valid MP4 (it needs
/// to write the trailer on SIGINT, not SIGKILL).
struct ChildGuard {
    child: Option<Child>,
    name: &'static str,
}

impl ChildGuard {
    fn new(child: Child, name: &'static str) -> Self {
        Self {
            child: Some(child),
            name,
        }
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(c) => Ok(c.try_wait()?),
            None => Ok(None),
        }
    }

    fn drain_stderr(&mut self) -> String {
        let Some(child) = self.child.as_mut() else {
            return String::new();
        };
        let Some(mut s) = child.stderr.take() else {
            return String::new();
        };
        let mut buf = String::new();
        let _ = s.read_to_string(&mut buf);
        buf.trim().to_string()
    }

    /// SIGINT, then wait. If still alive after GRACEFUL_KILL_TIMEOUT,
    /// SIGKILL.
    fn interrupt_then_wait(&mut self) -> Result<std::process::ExitStatus> {
        let Some(mut child) = self.child.take() else {
            bail!("{}: already reaped", self.name);
        };
        send_signal(&child, libc::SIGINT);
        let deadline = Instant::now() + GRACEFUL_KILL_TIMEOUT;
        loop {
            match child.try_wait()? {
                Some(s) => return Ok(s),
                None => {
                    if Instant::now() >= deadline {
                        tracing::warn!("{}: didn't exit after SIGINT, sending SIGKILL", self.name);
                        let _ = child.kill();
                        return Ok(child.wait()?);
                    }
                    std::thread::sleep(Duration::from_millis(80));
                }
            }
        }
    }

    /// Immediate kill — for the Cancel path. Sends SIGKILL after a
    /// short SIGINT grace.
    fn kill_now(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        send_signal(&child, libc::SIGINT);
        let deadline = Instant::now() + Duration::from_millis(400);
        loop {
            match child.try_wait()? {
                Some(_) => return Ok(()),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(());
                }
                None => std::thread::sleep(Duration::from_millis(40)),
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        send_signal(&child, libc::SIGINT);
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return,
            }
        }
    }
}

fn send_signal(child: &Child, sig: libc::c_int) {
    let pid = child.id() as libc::pid_t;
    if pid > 0 {
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

// ───────────────────────── helpers ──────────────────────────────────

fn require_tool(name: &str) -> Result<()> {
    if which(name).is_some() {
        Ok(())
    } else {
        bail!("`{name}` is not installed. Install it with: sudo pacman -S {name}")
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn ensure_cache_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .ok_or_else(|| anyhow!("no XDG cache dir"))?
        .join("jot")
        .join("recordings");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// `~/Videos/gif/`. Created on first use. Honors `XDG_VIDEOS_DIR`.
pub fn ensure_gif_output_dir() -> Result<PathBuf> {
    let videos = dirs::video_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Videos")))
        .ok_or_else(|| anyhow!("no Videos dir"))?;
    let dir = videos.join("gif");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn file_timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string()
}

/// Spawn `wl-copy --type image/gif < gif`. Best-effort — failures only
/// log, since the user can always Save As manually.
pub fn copy_gif_to_clipboard(gif: &Path) -> Result<()> {
    if which("wl-copy").is_none() {
        bail!("wl-copy not installed (sudo pacman -S wl-clipboard)");
    }
    let mut child = Command::new("wl-copy")
        .args(["--type", "image/gif"])
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn wl-copy")?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("wl-copy stdin missing"))?;
        let mut f = std::fs::File::open(gif).context("open gif for clipboard")?;
        std::io::copy(&mut f, stdin)?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("wl-copy exited with {status}");
    }
    Ok(())
}

/// Reveal the gif in the user's file manager / image viewer via
/// `xdg-open`. Non-blocking.
pub fn open_path(path: &Path) -> Result<()> {
    Command::new("xdg-open")
        .arg(path.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("xdg-open")?;
    Ok(())
}

/// Returns a human-readable list of any missing tools.
pub fn missing_tools_summary() -> Option<String> {
    let missing: Vec<&str> = ["slurp", "wf-recorder", "ffmpeg"]
        .into_iter()
        .filter(|t| which(t).is_none())
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "Missing: {}. Install: sudo pacman -S {}",
            missing.join(", "),
            missing.join(" ")
        ))
    }
}

fn quality_crf(q: crate::config::GifQuality) -> u32 {
    use crate::config::GifQuality::*;
    match q {
        High => 17,
        Balanced => 22,
        Compact => 28,
    }
}

fn quality_palette_stats(q: crate::config::GifQuality) -> &'static str {
    use crate::config::GifQuality::*;
    match q {
        High => "full",
        Balanced | Compact => "diff",
    }
}

fn quality_dither(q: crate::config::GifQuality) -> &'static str {
    use crate::config::GifQuality::*;
    match q {
        High | Balanced => "sierra2_4a",
        Compact => "bayer:bayer_scale=3",
    }
}
