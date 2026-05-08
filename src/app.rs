use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::maintenance;
use crate::window::JotWindow;

pub const APP_ID: &str = "com.amageweb.Jot";

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::FLAGS_NONE)
        .build();

    app.connect_startup(|_| {
        // CSS is loaded by `ThemeController::install` inside JotWindow::build,
        // so the stylesheet matches the user's saved theme preference.
        if let Err(e) = maintenance::ensure_daily_backup() {
            tracing::warn!("daily backup skipped: {e}");
        }
    });

    let window: Rc<RefCell<Option<Rc<JotWindow>>>> = Rc::new(RefCell::new(None));

    let win_ref = window.clone();
    app.connect_activate(move |app| {
        let mut slot = win_ref.borrow_mut();
        match slot.as_ref() {
            Some(existing) => {
                // Toggle: if visible, hide; else show
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
