use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::compare_editor::launch_compare_editor;
use crate::maintenance;
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
            let main = win_ref.borrow().as_ref().map(|w| w.window.clone());
            launch_recorder(app, main);
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
                let main = win_ref.borrow().as_ref().map(|w| w.window.clone());
                launch_compare_editor(app, main, before.clone(), after.clone());
            } else {
                eprintln!("jot --compare needs two files: jot --compare antes.gif depois.gif");
            }
        } else {
            app.activate();
        }
        glib::ExitCode::SUCCESS
    });

    let win_ref = window.clone();
    app.connect_activate(move |app| {
        let mut slot = win_ref.borrow_mut();
        match slot.as_ref() {
            Some(existing) => {
                if existing.window.is_visible() {
                    existing.window.set_visible(false);
                } else {
                    existing.window.set_visible(true);
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
