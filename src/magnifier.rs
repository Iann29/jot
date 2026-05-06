//! Circular magnifier widget for the image preview dialog.
//!
//! Renders a zoomed sub-region of a `gdk::Texture` directly via
//! `WidgetImpl::snapshot`: a rounded clip, a translate/scale transform that
//! places the focus point at the centre of the lens, and a single
//! `append_scaled_texture` call. This avoids the
//! `Viewport + Picture::set_size_request` route, where `ContentFit::Fill`
//! makes the picture honour the viewport's allocation rather than the
//! requested size.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, graphene, gsk};

mod imp {
    use super::*;

    pub struct MagnifierLens {
        pub source: RefCell<Option<gdk::Texture>>,
        pub focus_x: Cell<f32>,
        pub focus_y: Cell<f32>,
        pub zoom: Cell<f32>,
        pub radius: Cell<f32>,
    }

    impl Default for MagnifierLens {
        fn default() -> Self {
            Self {
                source: RefCell::new(None),
                focus_x: Cell::new(0.0),
                focus_y: Cell::new(0.0),
                zoom: Cell::new(2.0),
                radius: Cell::new(110.0),
            }
        }
    }

    #[::glib::object_subclass]
    impl ObjectSubclass for MagnifierLens {
        const NAME: &'static str = "JotMagnifierLens";
        type Type = super::MagnifierLens;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for MagnifierLens {}

    impl WidgetImpl for MagnifierLens {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let texture = match self.source.borrow().as_ref() {
                Some(t) => t.clone(),
                None => return,
            };
            let r = self.radius.get();
            let zoom = self.zoom.get();
            let fx = self.focus_x.get();
            let fy = self.focus_y.get();
            let tw = texture.width() as f32;
            let th = texture.height() as f32;

            let diameter = 2.0 * r;
            let bounds = graphene::Rect::new(0.0, 0.0, diameter, diameter);
            let rounded = gsk::RoundedRect::from_rect(bounds, r);

            // 1. Circular clip — everything we paint inside this push/pop
            //    pair is masked to the disc.
            snapshot.push_rounded_clip(&rounded);

            // 2. Place the focus pixel at the centre of the lens, then scale.
            //    The texture is drawn at its natural size at this transformed
            //    origin; the surrounding clip discards everything outside.
            snapshot.save();
            snapshot.translate(&graphene::Point::new(r - fx * zoom, r - fy * zoom));
            snapshot.scale(zoom, zoom);
            snapshot.append_scaled_texture(
                &texture,
                gsk::ScalingFilter::Trilinear,
                &graphene::Rect::new(0.0, 0.0, tw, th),
            );
            snapshot.restore();

            snapshot.pop();

            // 3. White ring on top of the disc to read as a separate object.
            let border_widths: [f32; 4] = [2.0, 2.0, 2.0, 2.0];
            let ring = gdk::RGBA::new(1.0, 1.0, 1.0, 0.55);
            let colors = [ring; 4];
            snapshot.append_border(&rounded, &border_widths, &colors);
        }
    }
}

glib::wrapper! {
    pub struct MagnifierLens(ObjectSubclass<imp::MagnifierLens>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl MagnifierLens {
    pub fn new(texture: &gdk::Texture, radius: f32, zoom: f32) -> Self {
        let lens: Self = glib::Object::new();
        let imp = lens.imp();
        imp.source.replace(Some(texture.clone()));
        imp.radius.set(radius);
        imp.zoom.set(zoom);
        let diameter = (radius * 2.0).round() as i32;
        lens.set_size_request(diameter, diameter);
        lens.set_can_target(false);
        lens
    }

    /// Set the focus point in **texture pixel coordinates** and request a
    /// redraw. The lens widget itself is positioned by its parent (overlay
    /// margins) — this only changes what is shown inside it.
    pub fn set_focus(&self, x: f32, y: f32) {
        let imp = self.imp();
        imp.focus_x.set(x);
        imp.focus_y.set(y);
        self.queue_draw();
    }
}
