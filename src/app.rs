use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::maintenance;
use crate::recorder_window::launch_recorder;
use crate::window::JotWindow;

pub const APP_ID: &str = "com.amageweb.Jot";

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
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
        let want_recorder = cmd_line
            .arguments()
            .iter()
            .skip(1)
            .any(|a| a.to_string_lossy() == "--record-gif");

        if want_recorder {
            let main = win_ref.borrow().as_ref().map(|w| w.window.clone());
            launch_recorder(app, main);
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
