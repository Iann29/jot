//! Focused before/after comparison editor (the pivot away from the freeform
//! canvas). Two clips play side by side (or stacked), each with a 1-D trim
//! bar to cut its head/tail. Layout, labels and quality are simple controls.
//! Export reuses the memory-safe Phase 0 compositor (`canvas::compose`).
//!
//! Why this and not a freeform canvas: the real job is "put before/after
//! together and send it". That needs trimming and a couple of layout
//! choices — not arbitrary 2-D drag/resize, which is fiddly to make feel
//! good. Trimming is 1-D (drag a handle left/right), which is easy and solid.
//!
//! Preview pipeline: each clip is decoded to a downscaled, fps-capped PNG
//! sequence via ffmpeg on a worker thread; the frames load as textures and a
//! shared playhead (advanced by a GLib timer) drives both previews in sync,
//! mirroring the export's Freeze behaviour (the shorter clip holds its last
//! frame). Frame count and resolution are capped so RAM stays bounded.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::canvas::{self, BeforeAfterOptions, LayerKind, OutputKind, Scene, Stack};
use crate::config::{Config, GifQuality};
use crate::gif_recorder::{copy_gif_to_clipboard, open_path};

// Preview decodes at the export's clip height (so it's WYSIWYG, not a blurry
// upscale) but caps the frame count so RAM stays bounded: fps is lowered for
// long clips to keep `fps * duration <= PREVIEW_MAX_FRAMES`.
const PREVIEW_MAX_H: i32 = 480;
const PREVIEW_FPS_MAX: f64 = 12.0;
const PREVIEW_MAX_FRAMES: i32 = 150; // ~150 × 480p frames ≈ a couple hundred MB

// ───────────────────────────── ClipPreview ───────────────────────────

mod preview_imp {
    use super::*;

    pub struct ClipPreview {
        pub frames: RefCell<Vec<gdk::Texture>>,
        pub source_duration: Cell<f64>,
        pub decode_fps: Cell<f64>,
        pub trim_start: Cell<f64>,
        pub trim_end: Cell<f64>,
        pub playhead: Cell<f64>,
    }

    impl Default for ClipPreview {
        fn default() -> Self {
            Self {
                frames: RefCell::new(Vec::new()),
                source_duration: Cell::new(0.0),
                decode_fps: Cell::new(PREVIEW_FPS_MAX),
                trim_start: Cell::new(0.0),
                trim_end: Cell::new(0.0),
                playhead: Cell::new(0.0),
            }
        }
    }

    #[::glib::object_subclass]
    impl ObjectSubclass for ClipPreview {
        const NAME: &'static str = "JotClipPreview";
        type Type = super::ClipPreview;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ClipPreview {}

    impl WidgetImpl for ClipPreview {
        fn measure(&self, orientation: gtk::Orientation, _for: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Horizontal => (160, 380, -1, -1),
                _ => (120, 260, -1, -1),
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let w = obj.width() as f32;
            let h = obj.height() as f32;
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            // Backdrop.
            snapshot.append_color(
                &gdk::RGBA::new(0.05, 0.05, 0.07, 1.0),
                &graphene::Rect::new(0.0, 0.0, w, h),
            );

            let frames = self.frames.borrow();
            if frames.is_empty() {
                let layout = obj.create_pango_layout(Some("Carregando…"));
                let (lw, lh) = layout.pixel_size();
                snapshot.save();
                snapshot.translate(&graphene::Point::new(
                    (w - lw as f32) / 2.0,
                    (h - lh as f32) / 2.0,
                ));
                snapshot.append_layout(&layout, &gdk::RGBA::new(0.6, 0.6, 0.65, 1.0));
                snapshot.restore();
                return;
            }

            // Freeze semantics: clamp the playhead to this clip's trimmed
            // length, so a short clip holds its last frame.
            let ts = self.trim_start.get();
            let te = self.trim_end.get();
            let trimmed = (te - ts).max(0.0001);
            let local = self.playhead.get().min(trimmed);
            let src_t = ts + local;
            let idx = ((src_t * self.decode_fps.get()).round() as usize).min(frames.len() - 1);
            let tex = &frames[idx];

            let tw = tex.width() as f32;
            let th = tex.height() as f32;
            let scale = (w / tw).min(h / th);
            let dw = tw * scale;
            let dh = th * scale;
            let ox = (w - dw) / 2.0;
            let oy = (h - dh) / 2.0;
            snapshot.append_scaled_texture(
                tex,
                gsk::ScalingFilter::Trilinear,
                &graphene::Rect::new(ox, oy, dw, dh),
            );
        }
    }
}

glib::wrapper! {
    pub struct ClipPreview(ObjectSubclass<preview_imp::ClipPreview>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ClipPreview {
    fn new() -> Self {
        let p: Self = glib::Object::new();
        p.set_hexpand(true);
        p.set_vexpand(true);
        p.set_overflow(gtk::Overflow::Hidden);
        p
    }

    fn set_frames(&self, frames: Vec<gdk::Texture>, duration: f64, fps: f64) {
        let imp = self.imp();
        imp.source_duration.set(duration);
        imp.decode_fps.set(fps.max(0.1));
        if imp.trim_end.get() <= 0.0 {
            imp.trim_end.set(duration);
        }
        imp.frames.replace(frames);
        self.queue_draw();
    }

    fn set_trim(&self, start: f64, end: f64) {
        let imp = self.imp();
        imp.trim_start.set(start);
        imp.trim_end.set(end);
        self.queue_draw();
    }

    fn set_playhead(&self, t: f64) {
        self.imp().playhead.set(t);
        self.queue_draw();
    }
}

impl Default for ClipPreview {
    fn default() -> Self {
        Self::new()
    }
}

// ────────────────────────────── TrimBar ──────────────────────────────
//
// A `gtk::DrawingArea` (Cairo) showing the source duration with two drag
// handles (in / out) and a playhead marker. Simpler than a full widget
// subclass since it repaints rarely.

struct TrimState {
    duration: f64,
    start: f64,
    end: f64,
    playhead: f64,
    drag: Option<u8>, // 0 = start handle, 1 = end handle
    drag_origin: f64, // handle time at drag-begin
}

type TrimCallback = Rc<RefCell<Option<Box<dyn Fn(f64, f64)>>>>;

pub struct TrimBar {
    area: gtk::DrawingArea,
    state: Rc<RefCell<TrimState>>,
    on_change: TrimCallback,
}

impl TrimBar {
    fn new() -> Rc<Self> {
        let area = gtk::DrawingArea::new();
        area.set_hexpand(true);
        area.set_content_height(40);
        area.set_margin_top(6);

        let state = Rc::new(RefCell::new(TrimState {
            duration: 0.0,
            start: 0.0,
            end: 0.0,
            playhead: 0.0,
            drag: None,
            drag_origin: 0.0,
        }));
        let on_change: TrimCallback = Rc::new(RefCell::new(None));

        // Draw.
        let st = state.clone();
        area.set_draw_func(move |_, cr, w, h| {
            let s = st.borrow();
            let wf = w as f64;
            let hf = h as f64;
            // Track.
            cr.set_source_rgba(0.16, 0.16, 0.2, 1.0);
            cr.rectangle(0.0, hf * 0.38, wf, hf * 0.24);
            let _ = cr.fill();
            if s.duration <= 0.0 {
                return;
            }
            let x0 = s.start / s.duration * wf;
            let x1 = s.end / s.duration * wf;
            // Selected region.
            cr.set_source_rgba(0.36, 0.7, 1.0, 0.30);
            cr.rectangle(x0, hf * 0.30, (x1 - x0).max(0.0), hf * 0.40);
            let _ = cr.fill();
            // Handles.
            cr.set_source_rgba(0.36, 0.7, 1.0, 1.0);
            cr.rectangle(x0 - 3.0, hf * 0.18, 6.0, hf * 0.64);
            let _ = cr.fill();
            cr.rectangle(x1 - 3.0, hf * 0.18, 6.0, hf * 0.64);
            let _ = cr.fill();
            // Playhead (freeze-clamped to the trimmed range).
            let pt = s.start + s.playhead.min((s.end - s.start).max(0.0));
            let px = pt / s.duration * wf;
            cr.set_source_rgba(1.0, 0.3, 0.55, 0.95);
            cr.rectangle(px - 1.0, 0.0, 2.0, hf);
            let _ = cr.fill();
        });

        let me = Rc::new(Self {
            area: area.clone(),
            state: state.clone(),
            on_change: on_change.clone(),
        });

        // Drag the nearer handle.
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        let st_begin = state.clone();
        let area_begin = area.clone();
        drag.connect_drag_begin(move |_, x, _| {
            let mut s = st_begin.borrow_mut();
            if s.duration <= 0.0 {
                return;
            }
            let wf = area_begin.width().max(1) as f64;
            let x0 = s.start / s.duration * wf;
            let x1 = s.end / s.duration * wf;
            let which = if (x - x0).abs() <= (x - x1).abs() {
                0
            } else {
                1
            };
            s.drag = Some(which);
            s.drag_origin = if which == 0 { s.start } else { s.end };
        });

        let st_upd = state.clone();
        let area_upd = area.clone();
        let cb_upd = on_change.clone();
        drag.connect_drag_update(move |_, dx, _| {
            let (start, end) = {
                let mut s = st_upd.borrow_mut();
                let Some(which) = s.drag else {
                    return;
                };
                if s.duration <= 0.0 {
                    return;
                }
                let wf = area_upd.width().max(1) as f64;
                let dt = dx / wf * s.duration;
                let min_gap = (s.duration * 0.02).max(0.05);
                if which == 0 {
                    s.start = (s.drag_origin + dt).clamp(0.0, s.end - min_gap);
                } else {
                    s.end = (s.drag_origin + dt).clamp(s.start + min_gap, s.duration);
                }
                (s.start, s.end)
            };
            area_upd.queue_draw();
            if let Some(cb) = cb_upd.borrow().as_ref() {
                cb(start, end);
            }
        });

        let st_end = state.clone();
        drag.connect_drag_end(move |_, _, _| {
            st_end.borrow_mut().drag = None;
        });
        area.add_controller(drag);

        me
    }

    fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    fn set_duration(&self, duration: f64) {
        let mut s = self.state.borrow_mut();
        s.duration = duration;
        if s.end <= 0.0 {
            s.end = duration;
        }
        drop(s);
        self.area.queue_draw();
    }

    fn set_playhead(&self, t: f64) {
        self.state.borrow_mut().playhead = t;
        self.area.queue_draw();
    }

    fn trims(&self) -> (f64, f64) {
        let s = self.state.borrow();
        (s.start, s.end)
    }

    fn set_on_change<F: Fn(f64, f64) + 'static>(&self, f: F) {
        *self.on_change.borrow_mut() = Some(Box::new(f));
    }
}

// ──────────────────────── preview decode worker ──────────────────────

static PREVIEW_SEQ: AtomicU64 = AtomicU64::new(0);

enum PreviewMsg {
    Loaded {
        which: usize,
        duration: f64,
        fps: f64,
        /// Temp dir to clean up after loading. `None` for still images,
        /// whose frame IS the user's own file (never delete it).
        dir: Option<PathBuf>,
        frames: Vec<PathBuf>,
    },
    Failed {
        which: usize,
        err: String,
    },
}

/// Extract a downscaled, fps-capped PNG sequence for `path` on a worker
/// thread and post the frame paths back. Textures are loaded on the main
/// thread (GdkTexture isn't `Send`).
fn spawn_decode(path: PathBuf, which: usize, tx: async_channel::Sender<PreviewMsg>) {
    std::thread::Builder::new()
        .name("jot-preview-decode".into())
        .spawn(move || {
            let msg = (|| -> Result<PreviewMsg, String> {
                // A still image is its own single frame — no extraction, and
                // we must NOT delete the user's file (dir = None).
                if canvas::is_image(&path) {
                    return Ok(PreviewMsg::Loaded {
                        which,
                        duration: 0.0,
                        fps: 1.0,
                        dir: None,
                        frames: vec![path.clone()],
                    });
                }
                let duration = canvas::probe(&path).map_err(|e| format!("{e:#}"))?.duration;
                // Adaptive fps: keep total frames under the cap for long clips.
                let fps = if duration > 0.0 {
                    (PREVIEW_MAX_FRAMES as f64 / duration).clamp(1.0, PREVIEW_FPS_MAX)
                } else {
                    PREVIEW_FPS_MAX
                };
                let base = dirs::cache_dir()
                    .ok_or_else(|| "no cache dir".to_string())?
                    .join("jot")
                    .join("preview");
                let seq = PREVIEW_SEQ.fetch_add(1, Ordering::Relaxed);
                let dir = base.join(format!("p{seq}"));
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

                let pattern = dir.join("f_%05d.png");
                let status = std::process::Command::new("ffmpeg")
                    .args(["-y", "-loglevel", "error", "-i"])
                    .arg(&path)
                    .args([
                        "-vf",
                        &format!("fps={fps:.4},scale=-2:{PREVIEW_MAX_H}"),
                        "-frames:v",
                        &PREVIEW_MAX_FRAMES.to_string(),
                    ])
                    .arg(&pattern)
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map_err(|e| e.to_string())?;
                if !status.success() {
                    return Err("ffmpeg frame extraction failed".into());
                }

                let mut frames: Vec<PathBuf> = std::fs::read_dir(&dir)
                    .map_err(|e| e.to_string())?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
                    .collect();
                frames.sort();
                if frames.is_empty() {
                    return Err("no frames extracted".into());
                }
                Ok(PreviewMsg::Loaded {
                    which,
                    duration,
                    fps,
                    dir: Some(dir),
                    frames,
                })
            })()
            .unwrap_or_else(|err| PreviewMsg::Failed { which, err });
            let _ = tx.send_blocking(msg);
        })
        .expect("spawn preview decode thread");
}

// ─────────────────────────── editor window ───────────────────────────

struct CompareEditor {
    window: adw::ApplicationWindow,
    before_path: PathBuf,
    after_path: PathBuf,
    columns: gtk::Box,
    previews: [ClipPreview; 2],
    trims: [Rc<TrimBar>; 2],
    layout_dd: gtk::DropDown,
    quality_dd: gtk::DropDown,
    labels_check: gtk::CheckButton,
    play_btn: gtk::Button,
    export_btn: gtk::Button,
    copy_btn: gtk::Button,
    status: gtk::Label,
    playhead: Cell<f64>,
    max_dur: Cell<f64>,
    timer: RefCell<Option<glib::SourceId>>,
    busy: Cell<bool>,
    main_window_ref: glib::WeakRef<adw::ApplicationWindow>,
}

pub fn launch_compare_editor(
    app: &adw::Application,
    main_window: Option<adw::ApplicationWindow>,
    before: PathBuf,
    after: PathBuf,
) {
    let main_weak: glib::WeakRef<adw::ApplicationWindow> = glib::WeakRef::new();
    if let Some(w) = main_window.as_ref() {
        main_weak.set(Some(w));
    }
    let ed = CompareEditor::build(app, main_weak, before, after);
    ed.start_decode();
    ed.start_playback();
    ed.window.present();
}

impl CompareEditor {
    fn build(
        app: &adw::Application,
        main_window_ref: glib::WeakRef<adw::ApplicationWindow>,
        before: PathBuf,
        after: PathBuf,
    ) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Jot — Antes / Depois")
            .default_width(900)
            .default_height(620)
            .build();

        let header = adw::HeaderBar::new();
        let play_btn = gtk::Button::from_icon_name("media-playback-pause-symbolic");
        play_btn.set_tooltip_text(Some("Pausar / tocar"));
        header.pack_start(&play_btn);

        let copy_btn = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy_btn.set_tooltip_text(Some("Copiar GIF"));
        let export_btn = gtk::Button::with_label("Exportar GIF");
        export_btn.add_css_class("suggested-action");
        header.pack_end(&export_btn);
        header.pack_end(&copy_btn);

        // Two clip columns (label + preview + trim bar).
        let columns = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        columns.set_hexpand(true);
        columns.set_vexpand(true);
        columns.set_margin_start(12);
        columns.set_margin_end(12);
        columns.set_margin_top(12);

        let previews = [ClipPreview::new(), ClipPreview::new()];
        let trims = [TrimBar::new(), TrimBar::new()];
        for (i, title) in ["Antes", "Depois"].into_iter().enumerate() {
            let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
            col.set_hexpand(true);
            col.set_vexpand(true);
            let lbl = gtk::Label::new(Some(title));
            lbl.add_css_class("heading");
            col.append(&lbl);
            col.append(&previews[i]);
            col.append(trims[i].widget());
            columns.append(&col);
        }

        // Still-image sides have nothing to trim — hide their trim bar.
        for (i, p) in [&before, &after].into_iter().enumerate() {
            if canvas::is_image(p) {
                trims[i].widget().set_visible(false);
            }
        }

        // Controls row.
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        controls.set_margin_start(12);
        controls.set_margin_end(12);
        controls.set_margin_top(8);
        controls.set_margin_bottom(4);

        let layout_dd = gtk::DropDown::from_strings(&["Lado a lado", "Empilhado"]);
        let quality_dd = gtk::DropDown::from_strings(&["Alta", "Equilibrada", "Compacta"]);
        quality_dd.set_selected(quality_index(Config::load().gif_quality));
        let labels_check = gtk::CheckButton::with_label("Labels Antes/Depois");
        labels_check.set_active(true);

        controls.append(&labeled("Layout:", &layout_dd));
        controls.append(&labeled("Qualidade:", &quality_dd));
        controls.append(&labels_check);

        let status = gtk::Label::new(Some("Arraste as alças pra cortar cada clipe."));
        status.add_css_class("dim-label");
        status.set_xalign(0.0);
        status.set_hexpand(true);
        status.set_margin_start(12);
        status.set_margin_bottom(8);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&columns);
        content.append(&controls);
        content.append(&status);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        window.set_content(Some(&toolbar));

        let ed = Rc::new(Self {
            window,
            before_path: before,
            after_path: after,
            columns,
            previews,
            trims,
            layout_dd: layout_dd.clone(),
            quality_dd,
            labels_check,
            play_btn: play_btn.clone(),
            export_btn: export_btn.clone(),
            copy_btn: copy_btn.clone(),
            status,
            playhead: Cell::new(0.0),
            max_dur: Cell::new(0.0),
            timer: RefCell::new(None),
            busy: Cell::new(false),
            main_window_ref,
        });

        // Wire trim bars → update the matching preview + recompute loop length.
        for i in 0..2 {
            let e = ed.clone();
            ed.trims[i].set_on_change(move |start, end| {
                e.previews[i].set_trim(start, end);
                e.recompute_max_dur();
            });
        }

        let e = ed.clone();
        play_btn.connect_clicked(move |_| e.toggle_play());
        let e = ed.clone();
        export_btn.connect_clicked(move |_| e.on_export(false));
        let e = ed.clone();
        copy_btn.connect_clicked(move |_| e.on_export(true));
        let e = ed.clone();
        layout_dd.connect_selected_notify(move |_| e.apply_layout());

        let e = ed.clone();
        ed.window.connect_close_request(move |_| {
            e.stop_playback();
            e.on_close();
            glib::Propagation::Proceed
        });

        ed
    }

    fn start_decode(self: &Rc<Self>) {
        let (tx, rx) = async_channel::unbounded::<PreviewMsg>();
        spawn_decode(self.before_path.clone(), 0, tx.clone());
        spawn_decode(self.after_path.clone(), 1, tx);

        let me = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    PreviewMsg::Loaded {
                        which,
                        duration,
                        fps,
                        dir,
                        frames,
                    } => {
                        let textures: Vec<gdk::Texture> = frames
                            .iter()
                            .filter_map(|p| {
                                gdk::Texture::from_file(&gtk::gio::File::for_path(p)).ok()
                            })
                            .collect();
                        if let Some(dir) = dir {
                            let _ = std::fs::remove_dir_all(&dir);
                        }
                        if !textures.is_empty() {
                            me.previews[which].set_frames(textures, duration, fps);
                            me.trims[which].set_duration(duration);
                            me.recompute_max_dur();
                        }
                    }
                    PreviewMsg::Failed { which, err } => {
                        tracing::warn!("preview {which} decode failed: {err}");
                        me.status
                            .set_text(&format!("Não consegui pré-visualizar um clipe: {err}"));
                    }
                }
            }
        });
    }

    fn recompute_max_dur(&self) {
        let mut m = 0.0_f64;
        for t in &self.trims {
            let (s, e) = t.trims();
            m = m.max(e - s);
        }
        self.max_dur.set(m);
    }

    fn start_playback(self: &Rc<Self>) {
        self.stop_playback();
        let me = self.clone();
        let id = glib::timeout_add_local(
            Duration::from_millis((1000.0 / PREVIEW_FPS_MAX) as u64),
            move || {
                let max = me.max_dur.get();
                if max > 0.0 {
                    // Advance, then hold ~0.6s at the end before looping so the
                    // viewer can read the final state (mirrors a GIF restart).
                    let mut t = me.playhead.get() + 1.0 / PREVIEW_FPS_MAX;
                    if t > max + 0.6 {
                        t = 0.0;
                    }
                    me.playhead.set(t);
                    for p in &me.previews {
                        p.set_playhead(t);
                    }
                    for tb in &me.trims {
                        tb.set_playhead(t);
                    }
                }
                glib::ControlFlow::Continue
            },
        );
        *self.timer.borrow_mut() = Some(id);
    }

    fn stop_playback(&self) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
    }

    fn toggle_play(self: &Rc<Self>) {
        if self.timer.borrow().is_some() {
            self.stop_playback();
            self.play_btn.set_icon_name("media-playback-start-symbolic");
        } else {
            self.start_playback();
            self.play_btn.set_icon_name("media-playback-pause-symbolic");
        }
    }

    fn apply_layout(&self) {
        let orientation = if self.layout_dd.selected() == 0 {
            gtk::Orientation::Horizontal
        } else {
            gtk::Orientation::Vertical
        };
        self.columns.set_orientation(orientation);
    }

    fn current_options(&self) -> BeforeAfterOptions {
        let cfg = Config::load();
        let stack = if self.layout_dd.selected() == 0 {
            Stack::Horizontal
        } else {
            Stack::Vertical
        };
        let labels = if self.labels_check.is_active() {
            Some(("Antes".to_string(), "Depois".to_string()))
        } else {
            None
        };
        BeforeAfterOptions {
            fps: cfg.gif_fps,
            stack,
            labels,
            ..Default::default()
        }
    }

    /// Build the export scene: the before/after template with each clip's
    /// trim applied and the scene duration set to the longer trimmed clip.
    fn build_scene(&self) -> anyhow::Result<Scene> {
        let opts = self.current_options();
        let mut scene = canvas::before_after(&self.before_path, &self.after_path, &opts)?;
        let trims = [self.trims[0].trims(), self.trims[1].trims()];
        // The two media layers are always indices 0 (before) and 1 (after);
        // apply each side's trim only when that side is a clip. Image sides
        // have no trim bar and are left untouched.
        let mut maxdur = 0.0_f64;
        for (i, layer) in scene.layers.iter_mut().enumerate().take(2) {
            if let LayerKind::Clip {
                trim_start,
                trim_end,
                ..
            } = &mut layer.kind
            {
                let (s, e) = trims[i];
                *trim_start = s;
                *trim_end = Some(e);
                maxdur = maxdur.max(e - s);
            }
        }
        if maxdur > 0.0 {
            scene.canvas.duration = maxdur;
        }
        Ok(scene)
    }

    fn on_export(self: &Rc<Self>, to_clipboard: bool) {
        if self.busy.get() {
            return;
        }
        let scene = match self.build_scene() {
            Ok(s) => s,
            Err(e) => {
                self.status
                    .set_text(&format!("Erro ao montar a cena: {e:#}"));
                return;
            }
        };
        let quality = quality_from_index(self.quality_dd.selected());

        self.busy.set(true);
        self.export_btn.set_sensitive(false);
        self.copy_btn.set_sensitive(false);
        self.status.set_text(if to_clipboard {
            "Gerando e copiando…"
        } else {
            "Exportando GIF…"
        });

        let (tx, rx) = async_channel::unbounded::<Result<PathBuf, String>>();
        std::thread::Builder::new()
            .name("jot-compare-export".into())
            .spawn(move || {
                let res = (|| -> anyhow::Result<PathBuf> {
                    let out = canvas::default_output_path(OutputKind::Gif)?;
                    canvas::compose(&scene, OutputKind::Gif, quality, &out)?;
                    Ok(out)
                })()
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send_blocking(res);
            })
            .expect("spawn export thread");

        let me = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(res) = rx.recv().await {
                me.busy.set(false);
                me.export_btn.set_sensitive(true);
                me.copy_btn.set_sensitive(true);
                match res {
                    Ok(path) => {
                        if to_clipboard {
                            match copy_gif_to_clipboard(&path) {
                                Ok(()) => {
                                    me.status.set_text("✓ Copiado para a área de transferência")
                                }
                                Err(e) => me
                                    .status
                                    .set_text(&format!("Gerado, mas falha ao copiar: {e}")),
                            }
                        } else {
                            let kb = std::fs::metadata(&path)
                                .map(|m| m.len() / 1024)
                                .unwrap_or(0);
                            me.status.set_text(&format!(
                                "✓ {} · {} KB",
                                path.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                kb
                            ));
                            let _ = open_path(&path);
                        }
                    }
                    Err(msg) => {
                        tracing::error!("compare export failed: {msg}");
                        me.status.set_text(&format!("Erro ao exportar: {msg}"));
                    }
                }
            }
        });
    }

    fn on_close(&self) {
        if let Some(main) = self.main_window_ref.upgrade() {
            main.present();
        } else if let Some(app) = self.window.application() {
            app.quit();
        }
    }
}

fn labeled(text: &str, child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let l = gtk::Label::new(Some(text));
    l.add_css_class("dim-label");
    b.append(&l);
    b.append(child);
    b
}

fn quality_index(q: GifQuality) -> u32 {
    match q {
        GifQuality::High => 0,
        GifQuality::Balanced => 1,
        GifQuality::Compact => 2,
    }
}

fn quality_from_index(i: u32) -> GifQuality {
    match i {
        0 => GifQuality::High,
        2 => GifQuality::Compact,
        _ => GifQuality::Balanced,
    }
}
