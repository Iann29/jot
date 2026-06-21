//! Scene model + offline ffmpeg compositor — the engine behind the GIF
//! "canvas" (before/after comparisons today, freeform editor later).
//!
//! Design (Phase 0):
//!
//!   Scene { canvas, layers }   ── a pure, serde-serialisable description
//!     │                            of a composition. No GTK, no I/O.
//!     ▼  build_filter()
//!   one ffmpeg `-filter_complex`  ── every layer maps onto an `overlay`
//!     │                            (clips/images) or `drawtext`/`drawbox`
//!     ▼  compose()                 (text/shapes). Palette pass at the end.
//!   ~/Videos/gif/jot-compare-….gif (or .mp4)
//!
//! The same `Scene` will later be produced interactively by the canvas
//! editor (Phases 1-3) and rendered live via GTK snapshot; this module is
//! the deterministic export path and is unit-testable without ffmpeg.
//!
//! Why overlay instead of hstack: a freeform canvas places each element at
//! an arbitrary `(x, y)`, which is exactly what `overlay` does. `hstack`
//! would lock us into a rigid grid and can't grow into the editor.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::GifQuality;

// ───────────────────────────── Scene model ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub canvas: Canvas,
    /// Back-to-front draw order: index 0 is the bottom layer.
    pub layers: Vec<Layer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// Background fill, `#RRGGBB` (web style; converted to ffmpeg `0x…`).
    pub background: String,
    pub fps: u32,
    /// Total output length in seconds. The GIF loops forever on top of it.
    pub duration: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    pub kind: LayerKind,
    pub transform: Transform,
    #[serde(default)]
    pub timing: Timing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayerKind {
    /// An animated source (gif/mp4) rendered at `transform` size.
    Clip {
        source: PathBuf,
        loop_mode: LoopMode,
        /// Seconds into the source where playback starts (cut from the head).
        #[serde(default)]
        trim_start: f64,
        /// Seconds into the source where playback ends. None = to the end.
        #[serde(default)]
        trim_end: Option<f64>,
    },
    /// A still image (png/jpg/…) held for the whole scene.
    Image { source: PathBuf },
    /// Vector text drawn via ffmpeg `drawtext`, centred inside the layer's
    /// `transform` box. (Phase 1+ will render this via Pango for fidelity.)
    Text {
        content: String,
        /// fontconfig family name, e.g. "Sans Bold".
        font: String,
        size: u32,
        /// `#RRGGBB`.
        color: String,
        /// Optional background box, `#RRGGBB` or `#RRGGBB@alpha`.
        #[serde(default)]
        box_color: Option<String>,
    },
    /// A solid filled rectangle (dividers, label bars, framing).
    Rect { color: String },
}

/// What a clip does when it's shorter than the scene.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoopMode {
    /// Repeat from the start until the scene ends.
    Loop,
    /// Hold the last frame until the scene ends. Best for before/after:
    /// both clips end together and the GIF restarts in sync.
    Freeze,
    /// Play once, then vanish (the background shows through).
    Once,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Transform {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Degrees clockwise. Reserved — ignored by the Phase 0 exporter.
    #[serde(default)]
    pub rotation: f64,
    /// 0.0–1.0.
    #[serde(default = "one")]
    pub opacity: f64,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Timing {
    /// Seconds from scene start before this layer appears.
    pub start: f64,
    /// How long the layer stays. None = until scene end.
    pub duration: Option<f64>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start: 0.0,
            duration: None,
        }
    }
}

// ──────────────────────────── media probing ──────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
}

/// Probe a media file's dimensions and duration via `ffprobe`. Falls back
/// to counting frames when the container omits a duration (common for
/// GIFs written by some encoders).
pub fn probe(path: &Path) -> Result<MediaInfo> {
    crate::gif_recorder::require_tool("ffprobe")?;
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,duration,avg_frame_rate:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("running ffprobe on {}", path.display()))?;
    if !out.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing ffprobe json")?;
    let stream = v
        .get("streams")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("no video stream in {}", path.display()))?;

    let width = json_u32(stream, "width")
        .ok_or_else(|| anyhow!("ffprobe: no width for {}", path.display()))?;
    let height = json_u32(stream, "height")
        .ok_or_else(|| anyhow!("ffprobe: no height for {}", path.display()))?;

    // Still images have no meaningful duration — we only need their size.
    let duration = if is_image(path) {
        0.0
    } else {
        json_f64(stream, "duration")
            .or_else(|| v.get("format").and_then(|f| json_f64(f, "duration")))
            .filter(|d| *d > 0.0)
            .or_else(|| count_frames_duration(path))
            .ok_or_else(|| anyhow!("could not determine duration of {}", path.display()))?
    };

    Ok(MediaInfo {
        width,
        height,
        duration,
    })
}

/// True for still-image extensions (treated as held frames, not clips).
pub fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp" | "bmp" | "avif" | "tiff")
    )
}

/// Last-resort duration: count decoded frames and divide by the average
/// frame rate. Slower (decodes the whole file) but always works.
fn count_frames_duration(path: &Path) -> Option<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames,avg_frame_rate",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let stream = v.get("streams")?.as_array()?.first()?;
    let frames = json_f64(stream, "nb_read_frames")?;
    let fps = stream
        .get("avg_frame_rate")
        .and_then(|r| r.as_str())
        .and_then(parse_rational)
        .filter(|f| *f > 0.0)?;
    Some(frames / fps)
}

fn parse_rational(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let n: f64 = n.parse().ok()?;
    let d: f64 = d.parse().ok()?;
    if d == 0.0 {
        None
    } else {
        Some(n / d)
    }
}

fn json_u32(v: &serde_json::Value, key: &str) -> Option<u32> {
    let f = v.get(key)?;
    f.as_u64()
        .map(|n| n as u32)
        .or_else(|| f.as_str().and_then(|s| s.parse().ok()))
}

fn json_f64(v: &serde_json::Value, key: &str) -> Option<f64> {
    let f = v.get(key)?;
    f.as_f64()
        .or_else(|| f.as_str().and_then(|s| s.parse().ok()))
}

// ───────────────────────── before/after template ─────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    /// Clips side by side (the classic before/after).
    Horizontal,
    /// Clips stacked top/bottom. Modelled now; exposed once the editor /
    /// CLI lets the user pick a layout (Phase 1+).
    #[allow(dead_code)]
    Vertical,
}

#[derive(Debug, Clone)]
pub struct BeforeAfterOptions {
    /// Height each clip is scaled to (horizontal) or width (vertical).
    pub target_size: u32,
    /// Pixels between the two clips.
    pub gap: u32,
    /// Outer padding around everything.
    pub padding: u32,
    /// Canvas background, `#RRGGBB`.
    pub background: String,
    /// Labels above each clip; None hides them entirely.
    pub labels: Option<(String, String)>,
    pub label_height: u32,
    pub label_font: String,
    pub label_size: u32,
    pub label_color: String,
    pub fps: u32,
    /// How the shorter clip behaves so both stay in sync.
    pub sync: LoopMode,
    pub stack: Stack,
}

impl Default for BeforeAfterOptions {
    fn default() -> Self {
        Self {
            target_size: 480,
            gap: 16,
            padding: 18,
            background: "#0e0e12".into(),
            labels: Some(("Antes".into(), "Depois".into())),
            label_height: 44,
            label_font: "Sans Bold".into(),
            label_size: 22,
            label_color: "#e8e8ec".into(),
            fps: 24,
            sync: LoopMode::Freeze,
            stack: Stack::Horizontal,
        }
    }
}

/// Build a side-by-side (or stacked) before/after `Scene` from two media
/// files. Probes both to size the canvas and to set the scene duration to
/// the longer of the two.
pub fn before_after(before: &Path, after: &Path, opts: &BeforeAfterOptions) -> Result<Scene> {
    let ia = probe(before)?;
    let ib = probe(after)?;

    let label_h = if opts.labels.is_some() {
        opts.label_height
    } else {
        0
    };
    let pad = opts.padding;
    let gap = opts.gap;
    let fps = opts.fps.max(1);
    // The longer clip drives the length. If both sides are still images there
    // is no clip to drive it, so fall back to a short static loop.
    let clip_dur = ia.duration.max(ib.duration);
    let duration = if clip_dur > 0.05 { clip_dur } else { 2.0 };

    // Per-clip display sizes, preserving aspect, snapped to even pixels
    // (libx264 needs even dimensions; harmless for GIF).
    let (wa, ha, wb, hb) = match opts.stack {
        Stack::Horizontal => {
            let h = even(opts.target_size);
            (even(scale_to_h(ia, h)), h, even(scale_to_h(ib, h)), h)
        }
        Stack::Vertical => {
            let w = even(opts.target_size);
            (w, even(scale_to_w(ia, w)), w, even(scale_to_w(ib, w)))
        }
    };

    // Positions of the two clips and the canvas size.
    let (canvas_w, canvas_h, (ax, ay), (bx, by)) = match opts.stack {
        Stack::Horizontal => {
            let cw = even(pad + wa + gap + wb + pad);
            let ch = even(pad + label_h + ha.max(hb) + pad);
            let ay = (pad + label_h) as i32;
            (cw, ch, (pad as i32, ay), ((pad + wa + gap) as i32, ay))
        }
        Stack::Vertical => {
            let cw = even(pad + wa.max(wb) + pad);
            let ch = even(pad + label_h + ha + gap + label_h + hb + pad);
            (
                cw,
                ch,
                (pad as i32, (pad + label_h) as i32),
                (pad as i32, (pad + label_h + ha + gap + label_h) as i32),
            )
        }
    };

    let mut layers = Vec::new();
    layers.push(media_layer(before, ax, ay, wa, ha, opts.sync));
    layers.push(media_layer(after, bx, by, wb, hb, opts.sync));

    if let Some((la, lb)) = &opts.labels {
        // Each label is centred in a box spanning its clip's width, sitting
        // in the strip directly above that clip.
        let (la_y, lb_y) = match opts.stack {
            Stack::Horizontal => (pad as i32, pad as i32),
            Stack::Vertical => (pad as i32, (pad + label_h + ha + gap) as i32),
        };
        layers.push(text_layer(la, ax, la_y, wa, label_h, opts));
        layers.push(text_layer(lb, bx, lb_y, wb, label_h, opts));
    }

    Ok(Scene {
        canvas: Canvas {
            width: canvas_w,
            height: canvas_h,
            background: opts.background.clone(),
            fps,
            duration,
        },
        layers,
    })
}

/// A media layer for the before/after template: an `Image` for still files,
/// a `Clip` otherwise.
fn media_layer(src: &Path, x: i32, y: i32, w: u32, h: u32, sync: LoopMode) -> Layer {
    let kind = if is_image(src) {
        LayerKind::Image {
            source: src.to_path_buf(),
        }
    } else {
        LayerKind::Clip {
            source: src.to_path_buf(),
            loop_mode: sync,
            trim_start: 0.0,
            trim_end: None,
        }
    };
    Layer {
        kind,
        transform: Transform {
            x,
            y,
            w,
            h,
            rotation: 0.0,
            opacity: 1.0,
        },
        timing: Timing::default(),
    }
}

fn text_layer(text: &str, x: i32, y: i32, w: u32, h: u32, opts: &BeforeAfterOptions) -> Layer {
    Layer {
        kind: LayerKind::Text {
            content: text.to_string(),
            font: opts.label_font.clone(),
            size: opts.label_size,
            color: opts.label_color.clone(),
            box_color: None,
        },
        transform: Transform {
            x,
            y,
            w,
            h,
            rotation: 0.0,
            opacity: 1.0,
        },
        timing: Timing::default(),
    }
}

fn scale_to_h(info: MediaInfo, h: u32) -> u32 {
    if info.height == 0 {
        return h;
    }
    ((info.width as f64) * (h as f64) / (info.height as f64)).round() as u32
}

fn scale_to_w(info: MediaInfo, w: u32) -> u32 {
    if info.width == 0 {
        return w;
    }
    ((info.height as f64) * (w as f64) / (info.width as f64)).round() as u32
}

/// Round down to the nearest even number, floored at 2.
fn even(n: u32) -> u32 {
    n.max(2) & !1
}

// ───────────────────────── filtergraph builder ───────────────────────

#[derive(Debug, Clone, Copy)]
pub enum OutputKind {
    Gif,
    /// H.264 MP4 — smaller and smoother for messaging apps. The compositor
    /// already supports it; the CLI/editor will offer it as a toggle (Phase 1+).
    #[allow(dead_code)]
    Mp4,
}

impl OutputKind {
    fn ext(self) -> &'static str {
        match self {
            OutputKind::Gif => "gif",
            OutputKind::Mp4 => "mp4",
        }
    }
}

struct InputSpec {
    path: PathBuf,
    /// Args that must precede the `-i` (e.g. `-stream_loop -1`).
    pre_args: Vec<String>,
}

/// Build the ffmpeg input list, the `-filter_complex` string that produces
/// the flattened composite, and the label of that composite stream. Pure —
/// no I/O, so the generated graph is unit-testable.
///
/// This deliberately stops at the composite: palette generation runs as a
/// *separate* ffmpeg pass (see `compose`). Doing palettegen+paletteuse in
/// one graph forces the paletteuse branch to buffer every frame until the
/// palette is ready — which, with a non-terminating source, balloons RAM
/// until the OOM killer fires. Two passes keep memory flat.
fn build_composite_filter(scene: &Scene) -> (Vec<InputSpec>, String, String) {
    let fps = scene.canvas.fps.max(1);
    let dur = scene.canvas.duration;
    let bg = to_ffmpeg_color(&scene.canvas.background);
    let font = resolve_scene_font(scene);

    let mut inputs: Vec<InputSpec> = Vec::new();
    let mut stmts: Vec<String> = Vec::new();

    // `d={dur}` makes the base FINITE. overlay follows the base, so the
    // whole composite ends at `dur` and every filter sees EOF — without
    // this the graph never terminates.
    stmts.push(format!(
        "color=c={bg}:s={w}x{h}:r={fps}:d={dur:.3}[base]",
        w = scene.canvas.width,
        h = scene.canvas.height,
    ));
    let mut cur = String::from("base");

    for (li, layer) in scene.layers.iter().enumerate() {
        let t = &layer.transform;
        match &layer.kind {
            LayerKind::Clip {
                source,
                loop_mode,
                trim_start,
                trim_end,
            } => {
                let trimmed = *trim_start > 0.0 || trim_end.is_some();
                let idx = inputs.len();
                let mut pre = Vec::new();
                // -stream_loop only makes sense for an untrimmed clip; a
                // trimmed clip's segment is bounded by the trim filter.
                if *loop_mode == LoopMode::Loop && !trimmed {
                    pre.push("-stream_loop".into());
                    pre.push("-1".into());
                }
                inputs.push(InputSpec {
                    path: source.clone(),
                    pre_args: pre,
                });

                let prep = format!("v{li}");
                let mut chain = format!("[{idx}:v]");
                if trimmed {
                    match trim_end {
                        Some(te) => chain.push_str(&format!(
                            "trim=start={:.3}:end={:.3},setpts=PTS-STARTPTS,",
                            trim_start, te
                        )),
                        None => chain.push_str(&format!(
                            "trim=start={:.3},setpts=PTS-STARTPTS,",
                            trim_start
                        )),
                    }
                }
                chain.push_str(&format!(
                    "fps={fps},scale={w}:{h}:flags=lanczos,setsar=1,format=rgba",
                    w = t.w,
                    h = t.h,
                ));
                if t.opacity < 0.999 {
                    chain.push_str(&format!(",colorchannelmixer=aa={:.3}", t.opacity.max(0.0)));
                }
                if *loop_mode == LoopMode::Freeze {
                    chain.push_str(&format!(",tpad=stop_mode=clone:stop_duration={dur:.3}"));
                }
                chain.push_str(&timing_setpts(layer));
                stmts.push(format!("{chain}[{prep}]"));

                let out = format!("o{li}");
                stmts.push(format!(
                    "[{cur}][{prep}]overlay={x}:{y}:eof_action=repeat{en}[{out}]",
                    x = t.x,
                    y = t.y,
                    en = enable_suffix(layer),
                ));
                cur = out;
            }
            LayerKind::Image { source } => {
                let idx = inputs.len();
                inputs.push(InputSpec {
                    path: source.clone(),
                    pre_args: vec!["-loop".into(), "1".into()],
                });
                let prep = format!("v{li}");
                let mut chain = format!(
                    "[{idx}:v]fps={fps},scale={w}:{h}:flags=lanczos,setsar=1,format=rgba",
                    w = t.w,
                    h = t.h,
                );
                if t.opacity < 0.999 {
                    chain.push_str(&format!(",colorchannelmixer=aa={:.3}", t.opacity.max(0.0)));
                }
                chain.push_str(&timing_setpts(layer));
                stmts.push(format!("{chain}[{prep}]"));

                let out = format!("o{li}");
                stmts.push(format!(
                    "[{cur}][{prep}]overlay={x}:{y}:eof_action=repeat{en}[{out}]",
                    x = t.x,
                    y = t.y,
                    en = enable_suffix(layer),
                ));
                cur = out;
            }
            LayerKind::Text {
                content,
                font: family,
                size,
                color,
                box_color,
            } => {
                let out = format!("o{li}");
                let expr = drawtext_expr(content, family, *size, color, box_color, t, &font);
                stmts.push(format!("[{cur}]{expr}[{out}]"));
                cur = out;
            }
            LayerKind::Rect { color } => {
                let out = format!("o{li}");
                stmts.push(format!(
                    "[{cur}]drawbox=x={x}:y={y}:w={w}:h={h}:color={c}:t=fill[{out}]",
                    x = t.x,
                    y = t.y,
                    w = t.w,
                    h = t.h,
                    c = to_ffmpeg_color(color),
                ));
                cur = out;
            }
        }
    }

    // Flatten to a opaque RGB composite. The palette/encode is a later pass.
    let out = "comp".to_string();
    stmts.push(format!("[{cur}]format=rgb24[{out}]"));

    (inputs, stmts.join(";"), out)
}

fn timing_setpts(layer: &Layer) -> String {
    if layer.timing.start > 0.0 {
        format!(",setpts=PTS-STARTPTS+{:.3}/TB", layer.timing.start)
    } else {
        ",setpts=PTS-STARTPTS".to_string()
    }
}

fn enable_suffix(layer: &Layer) -> String {
    let start = layer.timing.start;
    match layer.timing.duration {
        Some(d) => format!(":enable='between(t,{start:.3},{:.3})'", start + d),
        None if start > 0.0 => format!(":enable='gte(t,{start:.3})'"),
        None => String::new(),
    }
}

fn drawtext_expr(
    content: &str,
    family: &str,
    size: u32,
    color: &str,
    box_color: &Option<String>,
    t: &Transform,
    resolved_font: &Option<PathBuf>,
) -> String {
    let mut s = String::from("drawtext=");
    match resolved_font {
        Some(p) => s.push_str(&format!("fontfile='{}':", p.display())),
        None => s.push_str(&format!("font='{}':", family)),
    }
    s.push_str(&format!("text='{}':", escape_drawtext(content)));
    s.push_str(&format!("fontsize={size}:"));
    s.push_str(&format!("fontcolor={}:", to_ffmpeg_color(color)));
    // Centre the text inside the layer's box using drawtext's text_w/text_h.
    s.push_str(&format!("x={x}+({w}-text_w)/2:", x = t.x, w = t.w));
    s.push_str(&format!("y={y}+({h}-text_h)/2", y = t.y, h = t.h));
    if let Some(bc) = box_color {
        s.push_str(&format!(
            ":box=1:boxcolor={}:boxborderw=10",
            to_ffmpeg_color(bc)
        ));
    }
    s
}

/// Escape a string for use inside `text='…'` in a filtergraph. We wrap in
/// single quotes, so the dangerous characters are the quote itself and the
/// backslash; we also neutralise `%` (drawtext expansion).
fn escape_drawtext(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        // A literal single quote can't live inside single quotes; swap for a
        // typographic apostrophe rather than fight ffmpeg's quoting rules.
        .replace('\'', "\u{2019}")
}

/// `#RRGGBB[@alpha]` → `0xRRGGBB[@alpha]`. Named colours pass through.
fn to_ffmpeg_color(c: &str) -> String {
    match c.strip_prefix('#') {
        Some(rest) => format!("0x{rest}"),
        None => c.to_string(),
    }
}

/// Resolve the first text layer's font family to a concrete file via
/// `fc-match`, so drawtext doesn't depend on ffmpeg being built with
/// fontconfig. None → fall back to the `font=` family keyword.
fn resolve_scene_font(scene: &Scene) -> Option<PathBuf> {
    let family = scene.layers.iter().find_map(|l| match &l.kind {
        LayerKind::Text { font, .. } => Some(font.clone()),
        _ => None,
    })?;
    resolve_font(&family)
}

fn resolve_font(family: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}", family])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(PathBuf::from(p))
    }
}

// ─────────────────────────────── compose ─────────────────────────────

/// Render a `Scene` to `out_path`.
///
/// Pipeline (memory-bounded by construction — no full-stream buffering):
///
///   pass A: composite all layers → intermediate H.264 MP4 (streamed)
///   pass B: intermediate → palette.png         (GIF only)
///   pass C: intermediate + palette → out.gif   (GIF only)
///
/// For MP4 output, pass A writes straight to `out_path` and B/C are skipped.
/// Intermediates live in the cache dir and are always cleaned up.
pub fn compose(
    scene: &Scene,
    kind: OutputKind,
    quality: GifQuality,
    out_path: &Path,
) -> Result<()> {
    crate::gif_recorder::require_tool("ffmpeg")?;

    let (inputs, filter, map) = build_composite_filter(scene);
    tracing::debug!("compose composite filter: {filter}");

    let crf = crate::gif_recorder::quality_crf(quality);
    let encode_args: Vec<String> = [
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-preset",
        "veryfast",
        "-crf",
        &crf.to_string(),
        "-movflags",
        "+faststart",
        "-an",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    // MP4: composite straight to the destination, done.
    if let OutputKind::Mp4 = kind {
        return run_composite(
            &inputs,
            &filter,
            &map,
            scene.canvas.duration,
            &encode_args,
            out_path,
        );
    }

    // GIF: composite to an intermediate, then two-pass palette.
    let work = ensure_work_dir()?;
    let ts = crate::gif_recorder::file_timestamp();
    let intermediate = work.join(format!("jot-compose-{ts}.mp4"));
    let palette = work.join(format!("jot-compose-{ts}-palette.png"));

    let result = (|| -> Result<()> {
        run_composite(
            &inputs,
            &filter,
            &map,
            scene.canvas.duration,
            &encode_args,
            &intermediate,
        )?;
        run_palettegen(&intermediate, &palette, scene.canvas.fps, quality)?;
        run_paletteuse(&intermediate, &palette, out_path, scene.canvas.fps, quality)?;
        Ok(())
    })();

    // Always clean up intermediates, even on failure.
    let _ = std::fs::remove_file(&intermediate);
    let _ = std::fs::remove_file(&palette);
    result
}

/// Pass A: run the composite filtergraph to a single encoded video.
fn run_composite(
    inputs: &[InputSpec],
    filter: &str,
    map: &str,
    duration: f64,
    encode_args: &[String],
    out: &Path,
) -> Result<()> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y").args(["-loglevel", "error"]);
    for inp in inputs {
        for a in &inp.pre_args {
            cmd.arg(a);
        }
        cmd.arg("-i").arg(&inp.path);
    }
    cmd.arg("-filter_complex").arg(filter);
    cmd.arg("-map").arg(format!("[{map}]"));
    cmd.arg("-t").arg(format!("{duration:.3}"));
    cmd.args(encode_args);
    cmd.arg(out);
    run_checked(cmd, "composite")
}

/// Pass B: build the GIF palette from the composite (bounded — palettegen
/// accumulates a histogram, not the frames).
fn run_palettegen(src: &Path, palette: &Path, fps: u32, quality: GifQuality) -> Result<()> {
    let stats = crate::gif_recorder::quality_palette_stats(quality);
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .args(["-loglevel", "error"])
        .arg("-i")
        .arg(src);
    cmd.args([
        "-vf",
        &format!("fps={fps},format=rgb24,palettegen=stats_mode={stats}"),
    ]);
    cmd.arg(palette);
    run_checked(cmd, "palettegen")
}

/// Pass C: apply the palette to the composite, writing the final GIF.
fn run_paletteuse(
    src: &Path,
    palette: &Path,
    out: &Path,
    fps: u32,
    quality: GifQuality,
) -> Result<()> {
    let dither = crate::gif_recorder::quality_dither(quality);
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y").args(["-loglevel", "error"]);
    cmd.arg("-i").arg(src).arg("-i").arg(palette);
    cmd.args([
        "-lavfi",
        &format!(
            "fps={fps},format=rgb24[v];[v][1:v]paletteuse=dither={dither}:diff_mode=rectangle"
        ),
    ]);
    cmd.arg(out);
    run_checked(cmd, "paletteuse")
}

fn run_checked(mut cmd: Command, stage: &str) -> Result<()> {
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("running ffmpeg ({stage})"))?;
    if !out.status.success() {
        bail!(
            "ffmpeg {stage} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn ensure_work_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .ok_or_else(|| anyhow!("no XDG cache dir"))?
        .join("jot")
        .join("compose");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// `~/Videos/gif/jot-compare-<timestamp>.<ext>`.
pub fn default_output_path(kind: OutputKind) -> Result<PathBuf> {
    let dir = crate::gif_recorder::ensure_gif_output_dir()?;
    let ts = crate::gif_recorder::file_timestamp();
    Ok(dir.join(format!("jot-compare-{ts}.{}", kind.ext())))
}

// ──────────────────────────────── tests ──────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scene() -> Scene {
        Scene {
            canvas: Canvas {
                width: 400,
                height: 200,
                background: "#101014".into(),
                fps: 20,
                duration: 5.0,
            },
            layers: vec![
                Layer {
                    kind: LayerKind::Clip {
                        source: PathBuf::from("/tmp/a.gif"),
                        loop_mode: LoopMode::Freeze,
                        trim_start: 0.0,
                        trim_end: None,
                    },
                    transform: Transform {
                        x: 10,
                        y: 20,
                        w: 180,
                        h: 160,
                        rotation: 0.0,
                        opacity: 1.0,
                    },
                    timing: Timing::default(),
                },
                Layer {
                    kind: LayerKind::Clip {
                        source: PathBuf::from("/tmp/b.gif"),
                        loop_mode: LoopMode::Loop,
                        trim_start: 0.0,
                        trim_end: None,
                    },
                    transform: Transform {
                        x: 210,
                        y: 20,
                        w: 180,
                        h: 160,
                        rotation: 0.0,
                        opacity: 1.0,
                    },
                    timing: Timing::default(),
                },
                Layer {
                    kind: LayerKind::Text {
                        content: "Antes".into(),
                        font: "Sans Bold".into(),
                        size: 22,
                        color: "#ffffff".into(),
                        box_color: None,
                    },
                    transform: Transform {
                        x: 10,
                        y: 0,
                        w: 180,
                        h: 20,
                        rotation: 0.0,
                        opacity: 1.0,
                    },
                    timing: Timing::default(),
                },
            ],
        }
    }

    #[test]
    fn serde_round_trip() {
        let scene = sample_scene();
        let json = serde_json::to_string(&scene).unwrap();
        let back: Scene = serde_json::from_str(&json).unwrap();
        assert_eq!(back.layers.len(), scene.layers.len());
        assert_eq!(back.canvas.width, 400);
    }

    #[test]
    fn builder_inputs_and_overlays() {
        let scene = sample_scene();
        let (inputs, filter, map) = build_composite_filter(&scene);

        // Two clip layers → two ffmpeg inputs.
        assert_eq!(inputs.len(), 2);
        // The looping clip carries -stream_loop; the freeze one does not.
        assert!(inputs[1].pre_args.iter().any(|a| a == "-stream_loop"));
        assert!(inputs[0].pre_args.is_empty());

        // Base canvas (FINITE via d=), two overlays, a drawtext, ending at
        // the flattened composite — never a palette in this graph.
        assert!(filter.contains("color=c=0x101014:s=400x200:r=20:d=5.000[base]"));
        assert_eq!(filter.matches("overlay=").count(), 2);
        assert!(filter.contains("drawtext="));
        assert!(filter.contains("format=rgb24[comp]"));
        assert!(!filter.contains("palettegen"));
        // The freeze clip holds its last frame for the scene duration.
        assert!(filter.contains("tpad=stop_mode=clone:stop_duration=5.000"));
        assert_eq!(map, "comp");
    }

    #[test]
    fn trim_emits_trim_filter_and_drops_stream_loop() {
        let mut scene = sample_scene();
        // Trim clip B (a Loop clip) to 1.5–4.0s.
        if let LayerKind::Clip {
            trim_start,
            trim_end,
            ..
        } = &mut scene.layers[1].kind
        {
            *trim_start = 1.5;
            *trim_end = Some(4.0);
        }
        let (inputs, filter, _) = build_composite_filter(&scene);
        // Trimmed → a trim filter, and no -stream_loop (the segment is bounded
        // by trim, not by looping the whole input).
        assert!(filter.contains("trim=start=1.500:end=4.000"));
        assert!(!inputs[1].pre_args.iter().any(|a| a == "-stream_loop"));
        // Clip A stays untrimmed → exactly one trim filter overall.
        assert_eq!(filter.matches("trim=start=").count(), 1);
    }

    #[test]
    fn base_source_is_finite() {
        // Regression guard: an infinite base makes palettegen never emit,
        // so the downstream buffer grows until OOM. The base MUST carry a
        // duration.
        let scene = sample_scene();
        let (_, filter, _) = build_composite_filter(&scene);
        assert!(filter.contains(":d=5.000[base]"));
    }

    #[test]
    fn color_conversion() {
        assert_eq!(to_ffmpeg_color("#0e0e12"), "0x0e0e12");
        assert_eq!(to_ffmpeg_color("#000000@0.5"), "0x000000@0.5");
        assert_eq!(to_ffmpeg_color("white"), "white");
    }

    #[test]
    fn even_rounds_down() {
        assert_eq!(even(481), 480);
        assert_eq!(even(480), 480);
        assert_eq!(even(0), 2);
    }
}
