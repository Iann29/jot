use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::compare_editor::launch_compare_editor;
use crate::maintenance;
#[cfg(unix)]
use crate::recorder_window::launch_recorder;
use crate::window::JotWindow;

pub const APP_ID: &str = "com.amageweb.Jot";

pub fn run() -> glib::ExitCode {
    // Allow a parallel dev instance via `JOT_APP_ID=… jot …` so a freshly
    // built binary isn't just forwarded (D-Bus single-instance) to an older
    // jot already holding the default name. Prod uses the default id.
    let app_id = std::env::var("JOT_APP_ID").unwrap_or_else(|_| APP_ID.to_string());
    let app = adw::Application::builder()
        .application_id(&app_id)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    app.connect_startup(|_| {
        // CSS is loaded by ThemeController inside JotWindow::build so it
        // matches the saved theme preference.
        if let Err(e) = maintenance::ensure_daily_backup() {
            tracing::warn!("daily backup skipped: {e}");
        }
    });

    let window: Rc<RefCell<Option<Rc<JotWindow>>>> = Rc::new(RefCell::new(None));

    // command-line handler picks between toggling the main window and
    // firing the recorder. HANDLES_COMMAND_LINE routes second-instance
    // launches via D-Bus, so `jot --record-gif` from a keybind always
    // reaches the running process.
    let win_ref = window.clone();
    app.connect_command_line(move |app, cmd_line| {
        let args: Vec<String> = cmd_line
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        if args.iter().any(|a| a == "--record-gif") {
            // The recorder is Wayland-only (slurp + wf-recorder). Keep the
            // arm on every platform so the if/else chain — and the --compare
            // arm below it — stays identical, and just swap the body.
            #[cfg(unix)]
            {
                let main = {
                    let slot = win_ref.borrow();
                    live(&slot).map(|w| w.window.clone())
                };
                launch_recorder(app, main);
            }
            #[cfg(not(unix))]
            {
                tracing::warn!("--record-gif is not supported on this platform");
            }
        } else if let Some(pos) = args.iter().position(|a| a == "--compare") {
            // `jot --compare <before> <after>` opens the comparison editor.
            // The two paths are the next non-flag args; relative ones resolve
            // against the caller's cwd (command lines arrive over D-Bus from a
            // remote instance).
            let cwd = cmd_line.cwd();
            let paths: Vec<PathBuf> = args[pos + 1..]
                .iter()
                .filter(|a| !a.starts_with("--"))
                .take(2)
                .map(|a| resolve_path(cwd.as_deref(), a))
                .collect();
            if let [before, after] = paths.as_slice() {
                let main = {
                    let slot = win_ref.borrow();
                    live(&slot).map(|w| w.window.clone())
                };
                launch_compare_editor(app, main, before.clone(), after.clone());
            } else {
                // Windows links the GUI subsystem, so stderr is a null
                // handle and this would be swallowed whole — the log file
                // is the only channel that survives there (see main.rs).
                #[cfg(unix)]
                eprintln!("jot --compare needs two files: jot --compare antes.gif depois.gif");
                #[cfg(windows)]
                tracing::error!(
                    "jot --compare needs two files: jot --compare before.gif after.gif"
                );
            }
        } else {
            app.activate();
        }
        glib::ExitCode::SUCCESS
    });

    let win_ref = window.clone();
    app.connect_activate(move |app| {
        let mut slot = win_ref.borrow_mut();
        // Windows: the X really destroys the window (finish_close_request →
        // Propagation::Proceed). gtk_window_destroy() removes it from the
        // application, but the Rc here keeps the GObject alive; re-showing a
        // destroyed toplevel is a GTK error and its X no longer works. Drop
        // the corpse so the None arm rebuilds. Never true on Linux — there
        // the window is only ever hidden and keeps its application.
        if slot
            .as_ref()
            .is_some_and(|w| w.window.application().is_none())
        {
            *slot = None;
        }
        match slot.as_ref() {
            Some(existing) => {
                // Linux: activate IS the Super+N toggle — the Hyprland
                // keybind relaunches jot and a second press hides.
                #[cfg(unix)]
                if existing.window.is_visible() {
                    existing.window.set_visible(false);
                } else {
                    existing.window.set_visible(true);
                    existing.window.present();
                }
                // Windows: relaunching jot.exe is the only way to reach a
                // running instance (no tray, no global hotkey), so activate
                // must always mean "bring to front". Hiding here would drop
                // the taskbar entry and strand a live process holding the DB
                // — the same failure the close/Esc gates in window.rs avoid.
                // Note is_visible() is gtk_widget_is_visible and stays true
                // while minimized, so the toggle would also swallow an
                // Esc-minimized window.
                #[cfg(windows)]
                {
                    existing.window.set_visible(true);
                    existing.window.unminimize();
                    existing.window.present();
                }
            }
            None => {
                let w = JotWindow::build(app);
                w.window.present();
                *slot = Some(w);
            }
        }
    });

    app.run()
}

/// The main-window slot, filtered for liveness: a destroyed window (Windows
/// close path) has been released by its application and must not be handed
/// out as a parent/return-target.
fn live(slot: &Option<Rc<JotWindow>>) -> Option<&Rc<JotWindow>> {
    slot.as_ref().filter(|w| w.window.application().is_some())
}

/// Resolve a possibly-relative CLI path against the command line's cwd.
fn resolve_path(cwd: Option<&Path>, arg: &str) -> PathBuf {
    let p = PathBuf::from(arg);
    if p.is_absolute() {
        return p;
    }
    match cwd {
        Some(dir) => dir.join(p),
        None => p,
    }
}
