//! Theme controller — swaps `dark.css` / `light.css` and follows the
//! system color scheme via `adw::StyleManager`.
//!
//! libadwaita's `set_color_scheme` only flips its own named-color
//! resolution (and the `.dark` style class). Our stylesheet is a flat
//! palette of hex values, so we hold a per-instance `gtk::CssProvider`
//! and swap its content on theme change. Following the system means
//! also listening to `connect_dark_notify` so a user clicking
//! "Settings → Theme: System" still tracks runtime flips.

use std::cell::RefCell;
use std::rc::Rc;

#[allow(unused_imports)]
use adw::prelude::*;
use gtk::gdk;

use crate::config::Theme;

const DARK_CSS: &str = include_str!("dark.css");
const LIGHT_CSS: &str = include_str!("light.css");

pub struct ThemeController {
    provider: gtk::CssProvider,
    current: RefCell<Theme>,
}

impl ThemeController {
    /// Install the global stylesheet provider once at startup. Hooks
    /// `connect_dark_notify` so System mode follows runtime flips.
    pub fn install(initial: Theme) -> Rc<Self> {
        let provider = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let this = Rc::new(Self {
            provider,
            current: RefCell::new(initial),
        });

        let weak = Rc::downgrade(&this);
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(ctl) = weak.upgrade() {
                if matches!(*ctl.current.borrow(), Theme::System) {
                    ctl.refresh();
                }
            }
        });

        this.apply(initial);
        this
    }

    /// Set the user's preference and apply it.
    pub fn apply(&self, theme: Theme) {
        *self.current.borrow_mut() = theme;
        let sm = adw::StyleManager::default();
        match theme {
            // PreferDark on System lets the portal/desktop choose, while
            // still defaulting to dark on systems that don't expose a
            // preference (most tiling-WM users actually want dark).
            Theme::System => sm.set_color_scheme(adw::ColorScheme::PreferDark),
            Theme::Light => sm.set_color_scheme(adw::ColorScheme::ForceLight),
            Theme::Dark => sm.set_color_scheme(adw::ColorScheme::ForceDark),
        }
        self.refresh();
    }

    fn refresh(&self) {
        let dark = match *self.current.borrow() {
            Theme::Light => false,
            Theme::Dark => true,
            Theme::System => adw::StyleManager::default().is_dark(),
        };
        self.provider
            .load_from_string(if dark { DARK_CSS } else { LIGHT_CSS });
    }
}
