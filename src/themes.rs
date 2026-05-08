//! Theme controller — swaps `dark.css` / `light.css` and follows the
//! system color scheme via `adw::StyleManager`.
//!
//! libadwaita's `set_color_scheme` only flips its own named-color
//! resolution (and the `.dark` style class). Our stylesheet is a flat
//! palette of hex values, so we hold a per-instance `gtk::CssProvider`
//! and swap its content on theme change. Following the system means
//! also listening to `connect_dark_notify` so a user clicking
//! "Settings → Theme: System" still tracks runtime flips.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[allow(unused_imports)]
use adw::prelude::*;
use gtk::gdk;

use crate::config::Theme;

const DARK_CSS: &str = include_str!("dark.css");
const LIGHT_CSS: &str = include_str!("light.css");
// Background hexes used as the alpha base for the window-opacity slider.
// Must match the `window.jot-window { background-color: alpha(<hex>, …); }`
// rule at the top of each stylesheet.
const DARK_BG_HEX: &str = "#0f1014";
const LIGHT_BG_HEX: &str = "#f7f7fa";

/// Single source of truth for the active stylesheet AND the window
/// opacity. Opacity used to live in its own `gtk::CssProvider` at USER
/// priority, but that meant a hardcoded dark `background-color` rule
/// was always winning over the light stylesheet. Folding both into one
/// provider regenerates the CSS atomically whenever theme or opacity
/// changes, so the right palette is always on top.
pub struct ThemeController {
    provider: gtk::CssProvider,
    current: RefCell<Theme>,
    opacity: Cell<f64>,
}

impl ThemeController {
    pub fn install(initial: Theme, initial_opacity: f64) -> Rc<Self> {
        let provider = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            // USER priority so we win over libadwaita's named colours; we
            // ARE the app's theme.
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_USER,
            );
        }

        let this = Rc::new(Self {
            provider,
            current: RefCell::new(initial),
            opacity: Cell::new(initial_opacity),
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

    pub fn apply(&self, theme: Theme) {
        *self.current.borrow_mut() = theme;
        let sm = adw::StyleManager::default();
        match theme {
            // System on a tiling-WM box defaults dark, but PreferDark lets
            // the portal flip us if it exposes a preference.
            Theme::System => sm.set_color_scheme(adw::ColorScheme::PreferDark),
            Theme::Light => sm.set_color_scheme(adw::ColorScheme::ForceLight),
            Theme::Dark => sm.set_color_scheme(adw::ColorScheme::ForceDark),
        }
        self.refresh();
    }

    pub fn set_opacity(&self, opacity: f64) {
        self.opacity.set(opacity);
        self.refresh();
    }

    fn refresh(&self) {
        let dark = match *self.current.borrow() {
            Theme::Light => false,
            Theme::Dark => true,
            Theme::System => adw::StyleManager::default().is_dark(),
        };
        let base = if dark { DARK_CSS } else { LIGHT_CSS };
        let bg = if dark { DARK_BG_HEX } else { LIGHT_BG_HEX };
        let opacity = self.opacity.get().clamp(0.05, 1.0);
        // Append an override AFTER the base sheet so it wins on
        // last-rule-wins for `background-color`.
        let combined = format!(
            "{base}\n\
             /* opacity override (per-instance, regenerated on slider drag) */\n\
             window.jot-window {{ background-color: alpha({bg}, {opacity:.3}); }}\n"
        );
        self.provider.load_from_string(&combined);
    }
}
