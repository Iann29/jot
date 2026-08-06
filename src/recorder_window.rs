//! Mini floating overlay for the GIF recorder.
//!
//! Lives at the bottom-right of the screen (Hyprland window rule
//! positions it). Title is `Jot Recorder` so Hyprland can match on it
//! independently of the main jot window, which shares an `application-id`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::gif_recorder::{
    self, copy_gif_to_clipboard, missing_tools_summary, open_path, RecorderCmd, RecorderEvent,
    RecorderHandle,
};

const RECORDER_TITLE: &str = "Jot Recorder";

#[derive(Clone, Copy, PartialEq, Eq)]
enum UiState {
    Selecting,
    Recording,
    Converting,
    Done,
}

pub struct RecorderOverlay {
    window: adw::ApplicationWindow,
    status_label: gtk::Label,
    timer_label: gtk::Label,
    spinner: gtk::Spinner,
    button_box: gtk::Box,
    state: Cell<UiState>,
    handle: RefCell<Option<RecorderHandle>>,
    last_gif: RefCell<Option<PathBuf>>,
    main_window_ref: glib::WeakRef<adw::ApplicationWindow>,
}

/// Entry point used both by the header button and the `--record-gif` CLI flag.
pub fn launch_recorder(app: &adw::Application, main_window: Option<adw::ApplicationWindow>) {
    if let Some(msg) = missing_tools_summary() {
        if let Some(main) = main_window.as_ref() {
            let dialog = adw::AlertDialog::builder()
                .heading("GIF recorder needs a few tools")
                .body(&msg)
                .build();
            dialog.add_response("ok", "OK");
            dialog.set_default_response(Some("ok"));
            dialog.set_close_response("ok");
            dialog.present(Some(main));
        } else {
            eprintln!("jot --record-gif: {msg}");
        }
        return;
    }

    // Always show the placement picker before recording. The last
    // saved spot (if any) is used as the picker's starting position
    // so the user can just hit OK if it's already where they want.
    PlacementPicker::launch(app, main_window);
}

fn start_recording_session(app: &adw::Application, main_window: Option<adw::ApplicationWindow>) {
    let main_weak: glib::WeakRef<adw::ApplicationWindow> = glib::WeakRef::new();
    if let Some(w) = main_window.as_ref() {
        w.set_visible(false);
        main_weak.set(Some(w));
    }

    let overlay = RecorderOverlay::new(app, main_weak);
    overlay.start();
}

impl RecorderOverlay {
    fn new(
        app: &adw::Application,
        main_window_ref: glib::WeakRef<adw::ApplicationWindow>,
    ) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(RECORDER_TITLE)
            .resizable(false)
            .build();
        window.add_css_class("jot-recorder");
        // GtkWindow hardcodes a 360x200 minimum for toplevels; an
        // explicit size request is the only way to opt out of it, and
        // without it the bar can never shrink below 200px tall.
        window.set_size_request(1, 1);

        let red_dot = gtk::Label::builder().label("●").build();
        red_dot.add_css_class("jot-recorder-dot");

        let status_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .build();
        status_label.add_css_class("jot-recorder-status");
        status_label.set_visible(false);

        let timer_label = gtk::Label::builder()
            .label("0:00")
            .halign(gtk::Align::Start)
            .build();
        timer_label.add_css_class("jot-recorder-timer");

        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);

        let left = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        left.set_margin_start(8);
        left.set_margin_end(6);
        left.set_valign(gtk::Align::Center);
        left.set_hexpand(true);
        left.append(&red_dot);
        left.append(&spinner);
        left.append(&status_label);
        left.append(&timer_label);

        let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        button_box.set_margin_end(6);
        button_box.set_margin_top(4);
        button_box.set_margin_bottom(4);
        button_box.set_valign(gtk::Align::Center);
        button_box.set_halign(gtk::Align::End);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.append(&left);
        row.append(&button_box);

        window.set_content(Some(&row));

        Rc::new(Self {
            window,
            status_label,
            timer_label,
            spinner,
            button_box,
            state: Cell::new(UiState::Selecting),
            handle: RefCell::new(None),
            last_gif: RefCell::new(None),
            main_window_ref,
        })
    }

    fn start(self: &Rc<Self>) {
        let cfg = crate::config::Config::load();
        let (handle, evt_rx) = gif_recorder::start(cfg.gif_fps, cfg.gif_quality);
        *self.handle.borrow_mut() = Some(handle);

        let me = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(evt) = evt_rx.recv().await {
                if !me.handle_event(evt) {
                    break;
                }
            }
        });
    }

    fn handle_event(self: &Rc<Self>, evt: RecorderEvent) -> bool {
        match evt {
            RecorderEvent::SelectingRegion => {
                self.set_state(UiState::Selecting);
                true
            }
            RecorderEvent::RecordingStarted { region } => {
                tracing::info!("recording region {region}");
                self.set_state(UiState::Recording);
                self.timer_label.set_text("0:00");
                self.window.present();
                // Position came from the placement picker that ran
                // just before recording started; reuse the same
                // saved coords here so the live overlay lands where
                // the user just parked the picker.
                let cfg = crate::config::Config::load();
                let anchor = match (cfg.recorder_overlay_x, cfg.recorder_overlay_y) {
                    (Some(x), Some(y)) => Some((x, y)),
                    _ => pick_overlay_anchor(&region),
                };
                if let Some(a) = anchor {
                    schedule_place_on_map(&self.window, a, RECORDER_TITLE);
                }
                true
            }
            RecorderEvent::Tick { seconds } => {
                self.timer_label.set_text(&fmt_timer(seconds));
                true
            }
            RecorderEvent::Converting => {
                self.set_state(UiState::Converting);
                true
            }
            RecorderEvent::Done { gif_path, seconds } => {
                *self.last_gif.borrow_mut() = Some(gif_path.clone());
                let kb = std::fs::metadata(&gif_path)
                    .map(|m| m.len() / 1024)
                    .unwrap_or(0);
                // Keep the bar narrow: the filename would blow the
                // width past 500px, so it lives in the tooltip.
                self.status_label.set_text(&format!("{seconds}s · {kb} KB"));
                self.status_label
                    .set_tooltip_text(Some(&gif_path.to_string_lossy()));
                self.timer_label.set_text("");
                self.set_state(UiState::Done);
                true
            }
            RecorderEvent::Cancelled => {
                self.shutdown(None);
                false
            }
            RecorderEvent::Error(msg) => {
                self.shutdown(Some(msg));
                false
            }
        }
    }

    fn set_state(self: &Rc<Self>, state: UiState) {
        self.state.set(state);
        self.rebuild_buttons();
        match state {
            UiState::Selecting => {
                self.spinner.set_visible(false);
                self.spinner.stop();
                self.status_label.set_visible(false);
                self.timer_label.set_visible(false);
                self.window.remove_css_class("jot-recorder-converting");
                self.window.remove_css_class("jot-recorder-done");
                self.window.add_css_class("jot-recorder-recording");
            }
            UiState::Recording => {
                self.spinner.set_visible(false);
                self.spinner.stop();
                // Pulsing red dot + running timer say "recording"
                // without spending 70px on the word itself.
                self.status_label.set_visible(false);
                self.timer_label.set_visible(true);
                self.window.remove_css_class("jot-recorder-converting");
                self.window.remove_css_class("jot-recorder-done");
                self.window.add_css_class("jot-recorder-recording");
            }
            UiState::Converting => {
                self.spinner.set_visible(true);
                self.spinner.start();
                self.status_label.set_text("Converting…");
                self.status_label.set_visible(true);
                self.timer_label.set_visible(false);
                self.window.remove_css_class("jot-recorder-recording");
                self.window.remove_css_class("jot-recorder-done");
                self.window.add_css_class("jot-recorder-converting");
            }
            UiState::Done => {
                self.spinner.set_visible(false);
                self.spinner.stop();
                self.status_label.set_visible(true);
                self.timer_label.set_visible(false);
                self.window.remove_css_class("jot-recorder-recording");
                self.window.remove_css_class("jot-recorder-converting");
                self.window.add_css_class("jot-recorder-done");
                self.window.present();
            }
        }
    }

    fn rebuild_buttons(self: &Rc<Self>) {
        while let Some(child) = self.button_box.first_child() {
            self.button_box.remove(&child);
        }
        match self.state.get() {
            UiState::Selecting => {}
            UiState::Recording => {
                let stop = gtk::Button::builder()
                    .icon_name("media-playback-stop-symbolic")
                    .tooltip_text("Stop and save")
                    .build();
                stop.add_css_class("suggested-action");
                let me = self.clone();
                stop.connect_clicked(move |_| me.send_cmd(RecorderCmd::Stop));

                let cancel = gtk::Button::builder()
                    .icon_name("window-close-symbolic")
                    .tooltip_text("Cancel without saving")
                    .build();
                let me = self.clone();
                cancel.connect_clicked(move |_| me.send_cmd(RecorderCmd::Cancel));

                self.button_box.append(&stop);
                self.button_box.append(&cancel);
            }
            UiState::Converting => {
                let cancel = gtk::Button::builder()
                    .icon_name("window-close-symbolic")
                    .tooltip_text("Cancel conversion")
                    .build();
                let me = self.clone();
                cancel.connect_clicked(move |_| me.send_cmd(RecorderCmd::Cancel));
                self.button_box.append(&cancel);
            }
            UiState::Done => {
                let copy = gtk::Button::builder()
                    .icon_name("edit-copy-symbolic")
                    .tooltip_text("Copy GIF to clipboard")
                    .build();
                let me = self.clone();
                copy.connect_clicked(move |_| me.on_copy());

                let open = gtk::Button::builder()
                    .icon_name("folder-open-symbolic")
                    .tooltip_text("Open with default viewer")
                    .build();
                let me = self.clone();
                open.connect_clicked(move |_| me.on_open());

                let rerecord = gtk::Button::builder()
                    .icon_name("media-playlist-repeat-symbolic")
                    .tooltip_text("Record another")
                    .build();
                let me = self.clone();
                rerecord.connect_clicked(move |_| me.on_rerecord());

                let close = gtk::Button::builder()
                    .icon_name("window-close-symbolic")
                    .tooltip_text("Close")
                    .build();
                let me = self.clone();
                close.connect_clicked(move |_| me.shutdown(None));

                self.button_box.append(&copy);
                self.button_box.append(&open);
                self.button_box.append(&rerecord);
                self.button_box.append(&close);
            }
        }
    }

    fn send_cmd(self: &Rc<Self>, cmd: RecorderCmd) {
        if let Some(h) = self.handle.borrow().as_ref() {
            let tx = h.cmd_tx.clone();
            glib::spawn_future_local(async move {
                let _ = tx.send(cmd).await;
            });
        }
    }

    fn on_copy(self: &Rc<Self>) {
        let Some(p) = self.last_gif.borrow().clone() else {
            return;
        };
        match copy_gif_to_clipboard(&p) {
            Ok(()) => self.status_label.set_text("✓ Copied GIF to clipboard"),
            Err(e) => {
                tracing::warn!("copy failed: {e}");
                self.status_label.set_text(&format!("Copy failed: {e}"));
            }
        }
    }

    fn on_open(self: &Rc<Self>) {
        if let Some(p) = self.last_gif.borrow().clone() {
            let _ = open_path(&p);
        }
    }

    fn on_rerecord(self: &Rc<Self>) {
        *self.handle.borrow_mut() = None;
        *self.last_gif.borrow_mut() = None;
        self.status_label.set_tooltip_text(None);
        self.window.set_visible(false);
        self.start();
    }

    fn shutdown(self: &Rc<Self>, error: Option<String>) {
        if let Some(h) = self.handle.borrow().as_ref() {
            let tx = h.cmd_tx.clone();
            glib::spawn_future_local(async move {
                let _ = tx.send(RecorderCmd::Cancel).await;
            });
        }
        *self.handle.borrow_mut() = None;

        if let Some(msg) = error {
            tracing::error!("recorder: {msg}");
            let dialog = adw::AlertDialog::builder()
                .heading("GIF recording failed")
                .body(&msg)
                .build();
            dialog.add_response("ok", "OK");
            dialog.set_default_response(Some("ok"));
            dialog.set_close_response("ok");
            dialog.present(Some(&self.window));
        }

        let main_alive = self.main_window_ref.upgrade();
        if let Some(main) = main_alive.as_ref() {
            // Header-button entry point. Re-surface the main jot window
            // and let the application keep running because the user is
            // still using it.
            main.set_visible(true);
            main.present();
            self.window.close();
        } else {
            // `jot --record-gif` was a one-shot launch (no other windows
            // open). With HANDLES_COMMAND_LINE the GApplication stays
            // alive waiting for more activations, so we'd leak a
            // background process if we just closed the window. Quit
            // explicitly. `quit()` flushes pending events, so calling
            // `close()` first guarantees window destruction runs.
            self.window.close();
            if let Some(app) = self.window.application() {
                app.quit();
            }
        }
    }
}

fn fmt_timer(seconds: u64) -> String {
    let m = seconds / 60;
    let s = seconds % 60;
    format!("{m}:{s:02}")
}

/// Move a window so its top-left lands at `(x, y)`. Matched by title.
/// Fire-and-forget — non-Hyprland sessions are a no-op.
fn hyprctl_move(title: &str, x: i32, y: i32) {
    let _ = std::process::Command::new("hyprctl")
        .arg("dispatch")
        .arg("movewindowpixel")
        .arg(format!("exact {x} {y},title:^({title})$"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Move the window to `anchor` after Hyprland has actually registered
/// it. We tried `connect_map` and `is_mapped()` first, but both fire
/// inside GTK before the wayland surface is committed — so the
/// movewindowpixel dispatch arrives before Hyprland's openwindow
/// event and silently no-ops, leaving the overlay at Hyprland's
/// default-centered position. A pair of delayed dispatches (one fast,
/// one slow) cover both fast and slow open paths without being
/// perceptible to the user.
fn schedule_place_on_map(_window: &adw::ApplicationWindow, anchor: (i32, i32), title: &str) {
    let (x, y) = anchor;
    let t1 = title.to_string();
    let t2 = title.to_string();
    glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
        hyprctl_move(&t1, x, y);
    });
    glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
        hyprctl_move(&t2, x, y);
    });
}

/// Read the current top-left screen position of the window whose title
/// matches `title_match`. Uses hyprctl because Wayland clients can't
/// read their own absolute position from GTK. Returns None if hyprctl
/// is missing or the window isn't currently tracked.
fn hyprctl_read_position(title_match: &str) -> Option<(i32, i32)> {
    let out = std::process::Command::new("hyprctl")
        .args(["clients", "-j"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let arr: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    for c in arr.as_array()?.iter() {
        let title = c.get("title").and_then(|v| v.as_str())?;
        if title == title_match {
            let at = c.get("at")?.as_array()?;
            if at.len() < 2 {
                return None;
            }
            let x = at[0].as_i64()? as i32;
            let y = at[1].as_i64()? as i32;
            return Some((x, y));
        }
    }
    None
}

/// Parse slurp output `"X,Y WxH"` into `(x, y, w, h)`.
fn parse_region(s: &str) -> Option<(i32, i32, i32, i32)> {
    let mut parts = s.split_whitespace();
    let xy = parts.next()?;
    let wh = parts.next()?;
    let (xs, ys) = xy.split_once(',')?;
    let (ws, hs) = wh.split_once('x')?;
    Some((
        xs.parse().ok()?,
        ys.parse().ok()?,
        ws.parse().ok()?,
        hs.parse().ok()?,
    ))
}

/// Geometry of whichever monitor contains the given point (top-left of
/// the recording region). Falls back to the primary monitor.
fn monitor_geometry_for(x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
    let display = gtk::gdk::Display::default()?;
    let monitors = display.monitors();
    let n = monitors.n_items();
    let mut fallback: Option<(i32, i32, i32, i32)> = None;
    for i in 0..n {
        let item = monitors.item(i)?;
        let m = item.downcast::<gtk::gdk::Monitor>().ok()?;
        let g = m.geometry();
        let mg = (g.x(), g.y(), g.width(), g.height());
        if fallback.is_none() {
            fallback = Some(mg);
        }
        if x >= g.x() && x < g.x() + g.width() && y >= g.y() && y < g.y() + g.height() {
            return Some(mg);
        }
    }
    fallback
}

/// Choose the screen corner whose centre is farthest from the centre of
/// the recording region — gives the overlay maximum visual breathing
/// room and guarantees it doesn't sit on top of what we're filming.
fn pick_overlay_anchor(region: &str) -> Option<(i32, i32)> {
    let (rx, ry, rw, rh) = parse_region(region)?;
    let (mx, my, mw, mh) = monitor_geometry_for(rx + rw / 2, ry + rh / 2)?;

    // Overlay reserves enough room for the wider Done-state layout
    // (status + Copy/Open/Re-record/Close).
    const OVERLAY_W: i32 = 240;
    const OVERLAY_H: i32 = 40;
    const MARGIN: i32 = 20;

    let candidates = [
        (mx + MARGIN, my + MARGIN),                                   // TL
        (mx + mw - OVERLAY_W - MARGIN, my + MARGIN),                  // TR
        (mx + MARGIN, my + mh - OVERLAY_H - MARGIN),                  // BL
        (mx + mw - OVERLAY_W - MARGIN, my + mh - OVERLAY_H - MARGIN), // BR
    ];

    let rcx = rx + rw / 2;
    let rcy = ry + rh / 2;
    candidates
        .iter()
        .max_by_key(|(cx, cy)| {
            let ocx = cx + OVERLAY_W / 2;
            let ocy = cy + OVERLAY_H / 2;
            let dx = (ocx - rcx) as i64;
            let dy = (ocy - rcy) as i64;
            dx * dx + dy * dy
        })
        .copied()
}

// ─────────────────────── Placement picker ────────────────────────────
//
// One-time setup window: a compact bar telling the user to drag it
// with Super+click and click OK when it's where they want.
//
// Why bother with a separate picker instead of slurp -p:
//   * Hyprland's native Super+drag for floating windows is muscle
//     memory for the user; we don't introduce a new gesture.
//   * The picker is styled like the real overlay, so the user parks
//     roughly the footprint they'll get. Widths differ a little
//     between states, but what gets saved is the top-left corner —
//     which is exactly what the overlay is anchored by.
//   * Once placed, the absolute screen position is saved to config
//     and reused on every subsequent recording — no re-positioning
//     unless they hit Settings → Reset overlay position.

pub const PLACEMENT_TITLE: &str = "Jot Recorder Placement";

pub struct PlacementPicker {
    window: adw::ApplicationWindow,
}

impl PlacementPicker {
    pub fn launch(app: &adw::Application, main_window: Option<adw::ApplicationWindow>) {
        let main_weak: glib::WeakRef<adw::ApplicationWindow> = glib::WeakRef::new();
        if let Some(w) = main_window.as_ref() {
            w.set_visible(false);
            main_weak.set(Some(w));
        }
        let me = std::rc::Rc::new(Self::build(app, main_weak));
        me.window.present();
        // If we have a remembered spot, start the picker there so the
        // user can just hit OK to confirm; otherwise leave it at the
        // Hyprland-default center.
        let cfg = crate::config::Config::load();
        if let (Some(x), Some(y)) = (cfg.recorder_overlay_x, cfg.recorder_overlay_y) {
            schedule_place_on_map(&me.window, (x, y), PLACEMENT_TITLE);
        }
    }

    fn build(
        app: &adw::Application,
        main_window_ref: glib::WeakRef<adw::ApplicationWindow>,
    ) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(PLACEMENT_TITLE)
            .resizable(false)
            .build();
        window.add_css_class("jot-recorder");
        window.add_css_class("jot-recorder-placement");
        // Same GtkWindow 360x200 floor as the recorder overlay.
        window.set_size_request(1, 1);

        let label = gtk::Label::builder()
            .label("Super+drag")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .tooltip_text("Hold Super and drag this bar where you want it, then click OK")
            .build();
        label.add_css_class("jot-recorder-status");
        label.set_margin_start(9);
        label.set_margin_end(4);

        let ok = gtk::Button::builder()
            .label("OK")
            .tooltip_text("Save this position and start recording")
            .build();
        ok.add_css_class("suggested-action");

        let cancel = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Cancel")
            .build();

        let bbox = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bbox.set_margin_end(6);
        bbox.set_margin_top(4);
        bbox.set_margin_bottom(4);
        bbox.set_valign(gtk::Align::Center);
        bbox.append(&ok);
        bbox.append(&cancel);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.append(&label);
        row.append(&bbox);
        window.set_content(Some(&row));

        let app_for_ok = app.clone();
        let win_for_ok = window.clone();
        let main_ref_for_ok = main_window_ref.clone();
        ok.connect_clicked(move |_| {
            let pos = hyprctl_read_position(PLACEMENT_TITLE);
            if let Some((x, y)) = pos {
                let mut cfg = crate::config::Config::load();
                cfg.recorder_overlay_x = Some(x);
                cfg.recorder_overlay_y = Some(y);
                if let Err(e) = cfg.save() {
                    tracing::warn!("could not save overlay position: {e}");
                }
            } else {
                tracing::warn!(
                    "could not read placement window position via hyprctl — falling back to auto corner"
                );
            }
            win_for_ok.close();
            let main = main_ref_for_ok.upgrade();
            start_recording_session(&app_for_ok, main);
        });

        let win_for_cancel = window.clone();
        let main_ref_for_cancel = main_window_ref.clone();
        cancel.connect_clicked(move |_| {
            win_for_cancel.close();
            if let Some(main) = main_ref_for_cancel.upgrade() {
                main.set_visible(true);
                main.present();
            } else if let Some(app) = win_for_cancel.application() {
                // CLI-launched, no main window — quit the whole app or
                // we'd leak a zombie GTK process on the user's session.
                app.quit();
            }
        });

        // Escape closes the picker (same as Cancel).
        let win_for_esc = window.clone();
        let key_ctl = gtk::EventControllerKey::new();
        let cancel_for_esc = cancel.clone();
        key_ctl.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::Escape {
                cancel_for_esc.emit_clicked();
                let _ = win_for_esc;
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
        window.add_controller(key_ctl);

        Self { window }
    }
}
