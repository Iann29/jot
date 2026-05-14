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
            .default_width(320)
            .default_height(80)
            .resizable(false)
            .build();
        window.add_css_class("jot-recorder");

        let red_dot = gtk::Label::builder().label("●").build();
        red_dot.add_css_class("jot-recorder-dot");

        let status_label = gtk::Label::builder()
            .label("Recording")
            .halign(gtk::Align::Start)
            .build();
        status_label.add_css_class("jot-recorder-status");

        let timer_label = gtk::Label::builder()
            .label("0:00")
            .halign(gtk::Align::Start)
            .build();
        timer_label.add_css_class("jot-recorder-timer");

        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);

        let left = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        left.set_margin_start(12);
        left.set_margin_end(8);
        left.set_valign(gtk::Align::Center);
        left.set_hexpand(true);
        left.append(&red_dot);
        left.append(&spinner);
        left.append(&status_label);
        left.append(&timer_label);

        let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        button_box.set_margin_end(10);
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
            RecorderEvent::RecordingStarted { region, anchor } => {
                tracing::info!("recording region {region}, overlay anchor {anchor:?}");
                self.set_state(UiState::Recording);
                self.timer_label.set_text("0:00");
                self.window.present();
                place_overlay_at(anchor);
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
                self.status_label.set_text(&format!(
                    "✓ {} · {}s · {} KB",
                    gif_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    seconds,
                    kb
                ));
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
                self.status_label.set_text("Selecting region…");
                self.window.remove_css_class("jot-recorder-converting");
                self.window.remove_css_class("jot-recorder-done");
                self.window.add_css_class("jot-recorder-recording");
            }
            UiState::Recording => {
                self.spinner.set_visible(false);
                self.spinner.stop();
                self.status_label.set_text("Recording");
                self.window.remove_css_class("jot-recorder-converting");
                self.window.remove_css_class("jot-recorder-done");
                self.window.add_css_class("jot-recorder-recording");
            }
            UiState::Converting => {
                self.spinner.set_visible(true);
                self.spinner.start();
                self.status_label.set_text("Converting to GIF…");
                self.window.remove_css_class("jot-recorder-recording");
                self.window.remove_css_class("jot-recorder-done");
                self.window.add_css_class("jot-recorder-converting");
            }
            UiState::Done => {
                self.spinner.set_visible(false);
                self.spinner.stop();
                self.window.remove_css_class("jot-recorder-recording");
                self.window.remove_css_class("jot-recorder-converting");
                self.window.add_css_class("jot-recorder-done");
                self.window.set_default_size(560, 80);
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
        self.window.set_default_size(320, 80);
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

/// Move the recorder overlay so its top-left lands at `(x, y)`. The
/// window is matched by title ("Jot Recorder"). Fire-and-forget — if
/// hyprctl is missing (non-Hyprland session) or the dispatch fails,
/// the overlay keeps whatever position the compositor chose.
fn place_overlay_at(anchor: (i32, i32)) {
    let (x, y) = anchor;
    let _ = std::process::Command::new("hyprctl")
        .arg("dispatch")
        .arg("movewindowpixel")
        .arg(format!("exact {x} {y},title:^(Jot Recorder)$"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
