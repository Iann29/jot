//! Pannable, zoomable image canvas for the preview dialog.
//!
//! Behaviour matches the canvas mental model from Figma / Photoshop /
//! image viewers:
//!
//! - **Drag** with the primary button to pan.
//! - **Ctrl + scroll** to zoom in/out, anchored at the cursor (the pixel
//!   under the cursor stays fixed during the zoom).
//! - **Double-click** to reset to the initial fit.
//!
//! Implementation is a single custom `gtk::Widget` whose
//! `WidgetImpl::snapshot` renders the texture under a `translate(pan)`
//! + `scale(zoom)` transform — no nested Picture / Viewport, so the
//! GTK4 layout pipeline never fights us.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, graphene, gsk};

const MIN_ZOOM: f64 = 0.05;
const MAX_ZOOM: f64 = 32.0;
const SCROLL_STEP: f64 = 1.18; // ~18% per notch

mod imp {
    use super::*;

    pub struct ImageCanvas {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub zoom: Cell<f64>,
        pub pan_x: Cell<f64>,
        pub pan_y: Cell<f64>,
        pub pointer: Cell<(f64, f64)>,
        pub drag_start_pan: Cell<(f64, f64)>,
        pub initialized: Cell<bool>,
    }

    impl Default for ImageCanvas {
        fn default() -> Self {
            Self {
                texture: RefCell::new(None),
                zoom: Cell::new(1.0),
                pan_x: Cell::new(0.0),
                pan_y: Cell::new(0.0),
                pointer: Cell::new((0.0, 0.0)),
                drag_start_pan: Cell::new((0.0, 0.0)),
                initialized: Cell::new(false),
            }
        }
    }

    #[::glib::object_subclass]
    impl ObjectSubclass for ImageCanvas {
        const NAME: &'static str = "JotImageCanvas";
        type Type = super::ImageCanvas;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ImageCanvas {}

    impl WidgetImpl for ImageCanvas {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let texture = match self.texture.borrow().as_ref() {
                Some(t) => t.clone(),
                None => return,
            };

            // Lazy first-paint fit — once we know the widget's allocation,
            // pick a zoom that fits the texture and centre it.
            if !self.initialized.get() {
                self.obj().fit_to_view(&texture);
                self.initialized.set(true);
            }

            let tw = texture.width() as f32;
            let th = texture.height() as f32;
            let zoom = self.zoom.get() as f32;
            let pan_x = self.pan_x.get() as f32;
            let pan_y = self.pan_y.get() as f32;

            snapshot.save();
            snapshot.translate(&graphene::Point::new(pan_x, pan_y));
            snapshot.scale(zoom, zoom);
            snapshot.append_scaled_texture(
                &texture,
                gsk::ScalingFilter::Trilinear,
                &graphene::Rect::new(0.0, 0.0, tw, th),
            );
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct ImageCanvas(ObjectSubclass<imp::ImageCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ImageCanvas {
    pub fn new(texture: &gdk::Texture) -> Self {
        let canvas: Self = glib::Object::new();
        let imp = canvas.imp();
        imp.texture.replace(Some(texture.clone()));

        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        canvas.set_focusable(true);
        canvas.set_cursor_from_name(Some("grab"));
        // Crucial: clip our paint to the widget bounds so panning past the
        // edges doesn't bleed onto sibling widgets (e.g. the header bar).
        canvas.set_overflow(gtk::Overflow::Hidden);

        canvas.install_drag_pan();
        canvas.install_scroll_zoom();
        canvas.install_pointer_track();
        canvas.install_double_click_reset();

        canvas
    }

    /// Compute initial fit (texture fits inside the widget, never larger
    /// than 1×) and centre the result.
    pub fn fit_to_view(&self, texture: &gdk::Texture) {
        let imp = self.imp();
        let w = self.width() as f64;
        let h = self.height() as f64;
        if w <= 0.0 || h <= 0.0 {
            // Widget not allocated yet — skip; will retry on next snapshot.
            return;
        }
        let tw = texture.width() as f64;
        let th = texture.height() as f64;
        let fit = (w / tw).min(h / th).min(1.0);
        imp.zoom.set(fit);
        let drawn_w = tw * fit;
        let drawn_h = th * fit;
        imp.pan_x.set((w - drawn_w) / 2.0);
        imp.pan_y.set((h - drawn_h) / 2.0);
    }

    pub fn reset_view(&self) {
        let imp = self.imp();
        let texture = match imp.texture.borrow().as_ref() {
            Some(t) => t.clone(),
            None => return,
        };
        self.fit_to_view(&texture);
        self.queue_draw();
    }

    fn install_drag_pan(&self) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);

        let canvas = self.clone();
        drag.connect_drag_begin(move |_, _, _| {
            let imp = canvas.imp();
            imp.drag_start_pan.set((imp.pan_x.get(), imp.pan_y.get()));
            canvas.set_cursor_from_name(Some("grabbing"));
        });

        let canvas = self.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let imp = canvas.imp();
            let (start_x, start_y) = imp.drag_start_pan.get();
            imp.pan_x.set(start_x + dx);
            imp.pan_y.set(start_y + dy);
            canvas.queue_draw();
        });

        let canvas = self.clone();
        drag.connect_drag_end(move |_, _, _| {
            canvas.set_cursor_from_name(Some("grab"));
        });

        self.add_controller(drag);
    }

    fn install_scroll_zoom(&self) {
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL,
        );
        let canvas = self.clone();
        scroll.connect_scroll(move |controller, _dx, dy| {
            let event = match controller.current_event() {
                Some(e) => e,
                None => return glib::Propagation::Proceed,
            };
            let ctrl = event
                .modifier_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            if !ctrl {
                return glib::Propagation::Proceed;
            }

            let factor = if dy < 0.0 {
                SCROLL_STEP
            } else if dy > 0.0 {
                1.0 / SCROLL_STEP
            } else {
                return glib::Propagation::Proceed;
            };

            canvas.zoom_by(factor);
            glib::Propagation::Stop
        });
        self.add_controller(scroll);
    }

    fn install_pointer_track(&self) {
        let motion = gtk::EventControllerMotion::new();
        let canvas = self.clone();
        motion.connect_motion(move |_, x, y| {
            canvas.imp().pointer.set((x, y));
        });
        self.add_controller(motion);
    }

    fn install_double_click_reset(&self) {
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let canvas = self.clone();
        click.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                canvas.reset_view();
            }
        });
        self.add_controller(click);
    }

    /// Apply a zoom multiplier anchored at the current pointer position.
    /// Math: keep the pixel under the cursor stationary across the zoom.
    /// Given screen-pos s, world-pos w, and `s = pan + zoom * w`, after
    /// zooming by `f` we want `s_new = pan_new + (zoom * f) * w == s`.
    /// Solving: `pan_new = s - f * (s - pan)`.
    pub fn zoom_by(&self, factor: f64) {
        let imp = self.imp();
        let old_zoom = imp.zoom.get();
        let new_zoom = (old_zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (new_zoom - old_zoom).abs() < f64::EPSILON {
            return;
        }
        let actual_factor = new_zoom / old_zoom;

        let (px, py) = imp.pointer.get();
        let pan_x = imp.pan_x.get();
        let pan_y = imp.pan_y.get();

        imp.zoom.set(new_zoom);
        imp.pan_x.set(px - actual_factor * (px - pan_x));
        imp.pan_y.set(py - actual_factor * (py - pan_y));
        self.queue_draw();
    }

    pub fn zoom_in(&self) {
        self.zoom_by(SCROLL_STEP);
    }

    pub fn zoom_out(&self) {
        self.zoom_by(1.0 / SCROLL_STEP);
    }

    pub fn zoom_pct(&self) -> f64 {
        self.imp().zoom.get() * 100.0
    }
}
