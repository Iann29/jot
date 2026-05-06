use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Local, Utc};
use gtk::glib;
use gtk::{gdk, gio};

use crate::config::Config;
use crate::db::{images_dir, Db};
use crate::note::Note;

const AUTOSAVE_DEBOUNCE_MS: u32 = 400;

pub struct JotWindow {
    pub window: adw::ApplicationWindow,
    db: Db,
    list_box: gtk::ListBox,
    text_view: gtk::TextView,
    buffer: gtk::TextBuffer,
    title_label: gtk::Label,
    subtitle_label: gtk::Label,
    placeholder: gtk::Label,
    search_entry: gtk::SearchEntry,
    state: RefCell<State>,
    suppress: Cell<bool>,
    autosave: RefCell<Option<glib::SourceId>>,
}

struct State {
    notes: Vec<Note>,
    current_id: Option<i64>,
    config: Config,
    filter: String,
}

impl JotWindow {
    pub fn build(app: &adw::Application) -> Rc<Self> {
        let db = Db::open().expect("could not open database");
        let config = Config::load();

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Jot")
            .default_width(config.width)
            .default_height(config.height)
            .build();
        window.add_css_class("jot-window");

        // Header
        let header = adw::HeaderBar::builder()
            .show_end_title_buttons(false)
            .show_start_title_buttons(false)
            .build();
        header.add_css_class("jot-headerbar");
        header.set_title_widget(Some(&build_title(&app_subtitle())));

        // Buttons
        let new_btn = gtk::Button::builder()
            .icon_name("document-new-symbolic")
            .tooltip_text("New note  ·  Ctrl+N")
            .build();
        new_btn.add_css_class("jot-accent");

        let delete_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete current note  ·  Ctrl+D")
            .build();

        let settings_btn = gtk::MenuButton::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Settings")
            .build();

        let close_btn = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Hide window  ·  Esc")
            .build();

        header.pack_start(&new_btn);
        header.pack_end(&close_btn);
        header.pack_end(&settings_btn);
        header.pack_end(&delete_btn);

        // Sidebar
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search notes…")
            .build();
        search_entry.add_css_class("jot-search");

        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        list_box.add_css_class("jot-list");

        let scrolled_list = gtk::ScrolledWindow::builder()
            .child(&list_box)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.add_css_class("jot-sidebar");
        sidebar.append(&search_entry);
        sidebar.append(&scrolled_list);
        sidebar.set_size_request(240, -1);

        // Editor area
        let title_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .build();
        title_label.add_css_class("jot-title");

        let subtitle_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::Start)
            .build();
        subtitle_label.add_css_class("jot-subtitle");

        let title_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        title_box.set_margin_start(20);
        title_box.set_margin_top(10);
        title_box.set_margin_bottom(2);
        title_box.append(&title_label);
        title_box.append(&subtitle_label);

        let buffer = gtk::TextBuffer::new(None);
        let text_view = gtk::TextView::builder()
            .buffer(&buffer)
            .wrap_mode(gtk::WrapMode::WordChar)
            .pixels_above_lines(2)
            .pixels_below_lines(2)
            .left_margin(0)
            .right_margin(0)
            .top_margin(8)
            .bottom_margin(40)
            .build();
        text_view.add_css_class("jot-editor");

        let placeholder = gtk::Label::builder()
            .label("Pick a note or hit Ctrl+N to start")
            .build();
        placeholder.add_css_class("jot-placeholder");

        let editor_overlay = gtk::Overlay::new();
        let editor_scroll = gtk::ScrolledWindow::builder()
            .child(&text_view)
            .vexpand(true)
            .hexpand(true)
            .build();
        editor_overlay.set_child(Some(&editor_scroll));
        editor_overlay.add_overlay(&placeholder);
        placeholder.set_halign(gtk::Align::Center);
        placeholder.set_valign(gtk::Align::Center);

        let editor_shell = gtk::Box::new(gtk::Orientation::Vertical, 0);
        editor_shell.add_css_class("jot-editor-shell");
        editor_shell.append(&title_box);
        editor_shell.append(&editor_overlay);
        editor_shell.set_hexpand(true);

        // Split: sidebar + editor
        let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        split.append(&sidebar);
        split.append(&editor_shell);

        // Root layout
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&split);
        window.set_content(Some(&root));

        let this = Rc::new(JotWindow {
            window: window.clone(),
            db,
            list_box: list_box.clone(),
            text_view: text_view.clone(),
            buffer: buffer.clone(),
            title_label,
            subtitle_label,
            placeholder,
            search_entry: search_entry.clone(),
            state: RefCell::new(State {
                notes: Vec::new(),
                current_id: None,
                config,
                filter: String::new(),
            }),
            suppress: Cell::new(false),
            autosave: RefCell::new(None),
        });

        // Wire callbacks
        this.connect_callbacks(&new_btn, &delete_btn, &settings_btn, &close_btn);
        this.install_shortcuts();
        this.apply_opacity();
        this.refresh_notes();

        // If there are notes, select the first; otherwise show placeholder
        let first_id = {
            let state = this.state.borrow();
            state.notes.first().map(|n| n.id)
        };
        if let Some(first) = first_id {
            this.select_note(first);
        } else {
            this.update_placeholder();
        }

        // Persist size on resize
        let win = this.clone();
        window.connect_default_width_notify(move |w| {
            let mut st = win.state.borrow_mut();
            st.config.width = w.default_width();
        });
        let win = this.clone();
        window.connect_default_height_notify(move |w| {
            let mut st = win.state.borrow_mut();
            st.config.height = w.default_height();
        });

        // Persist config when window closes
        let win = this.clone();
        window.connect_close_request(move |_| {
            win.save_pending();
            let _ = win.state.borrow().config.save();
            glib::Propagation::Proceed
        });

        this
    }

    fn connect_callbacks(
        self: &Rc<Self>,
        new_btn: &gtk::Button,
        delete_btn: &gtk::Button,
        settings_btn: &gtk::MenuButton,
        close_btn: &gtk::Button,
    ) {
        // New note
        let win = self.clone();
        new_btn.connect_clicked(move |_| win.new_note());

        // Delete
        let win = self.clone();
        delete_btn.connect_clicked(move |_| win.delete_current());

        // Hide
        let win = self.clone();
        close_btn.connect_clicked(move |_| {
            win.save_pending();
            win.window.set_visible(false);
        });

        // Settings popover
        settings_btn.set_popover(Some(&self.build_settings_popover()));

        // List selection
        let win = self.clone();
        self.list_box.connect_row_selected(move |_, row| {
            if win.suppress.get() {
                return;
            }
            if let Some(row) = row {
                let id_opt = unsafe { row.data::<i64>("note-id") };
                if let Some(id_ptr) = id_opt {
                    let id = unsafe { *id_ptr.as_ref() };
                    win.select_note(id);
                }
            }
        });

        // Buffer changed → schedule autosave + update title + hide placeholder
        let win = self.clone();
        self.buffer.connect_changed(move |_| {
            if win.suppress.get() {
                return;
            }
            win.update_placeholder();
            win.schedule_autosave();
        });

        // Search filter
        let win = self.clone();
        self.search_entry.connect_search_changed(move |entry| {
            let text = entry.text().to_string();
            win.state.borrow_mut().filter = text;
            win.rebuild_list();
        });

        // Paste handler — try image first
        let win = self.clone();
        self.buffer.connect_paste_done(move |_, _| {
            // After GTK pastes, also try image; this is fallback
            // Actual image handling done via key controller below
            let _ = &win;
        });
    }

    fn install_shortcuts(self: &Rc<Self>) {
        let controller = gtk::EventControllerKey::new();

        let win = self.clone();
        controller.connect_key_pressed(move |_, keyval, _, modifier| {
            let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
            match keyval {
                gdk::Key::Escape => {
                    win.save_pending();
                    win.window.set_visible(false);
                    glib::Propagation::Stop
                }
                gdk::Key::n if ctrl => {
                    win.new_note();
                    glib::Propagation::Stop
                }
                gdk::Key::d if ctrl => {
                    win.delete_current();
                    glib::Propagation::Stop
                }
                gdk::Key::f if ctrl => {
                    win.search_entry.grab_focus();
                    glib::Propagation::Stop
                }
                gdk::Key::v if ctrl => {
                    if win.try_paste_image() {
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(controller);
    }

    fn build_settings_popover(self: &Rc<Self>) -> gtk::Popover {
        let popover = gtk::Popover::new();
        popover.add_css_class("jot-settings");

        let container = gtk::Box::new(gtk::Orientation::Vertical, 8);

        let opacity_label = gtk::Label::builder()
            .label("Window opacity")
            .halign(gtk::Align::Start)
            .build();
        let opacity = self.state.borrow().config.opacity;
        let opacity_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.30, 1.0, 0.01);
        opacity_scale.set_value(opacity);
        opacity_scale.set_hexpand(true);
        opacity_scale.set_draw_value(false);

        let win = self.clone();
        opacity_scale.connect_value_changed(move |scale| {
            let v = scale.value();
            win.state.borrow_mut().config.opacity = v;
            win.apply_opacity();
        });

        let font_label = gtk::Label::builder()
            .label("Font size")
            .halign(gtk::Align::Start)
            .build();
        let font_size = self.state.borrow().config.font_size as f64;
        let font_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 11.0, 22.0, 1.0);
        font_scale.set_value(font_size);
        font_scale.set_hexpand(true);
        font_scale.set_draw_value(true);
        font_scale.set_value_pos(gtk::PositionType::Right);

        let win = self.clone();
        font_scale.connect_value_changed(move |scale| {
            let v = scale.value() as u32;
            win.state.borrow_mut().config.font_size = v;
            win.apply_font_size();
        });

        let hint = gtk::Label::builder()
            .label("Toggle: Super+N  ·  Escape hides")
            .halign(gtk::Align::Start)
            .build();
        hint.add_css_class("jot-row-time");
        hint.set_margin_top(6);

        container.append(&opacity_label);
        container.append(&opacity_scale);
        container.append(&font_label);
        container.append(&font_scale);
        container.append(&hint);

        popover.set_child(Some(&container));
        popover
    }

    fn refresh_notes(&self) {
        let notes = self.db.list_notes().unwrap_or_default();
        self.state.borrow_mut().notes = notes;
        self.rebuild_list();
    }

    fn rebuild_list(&self) {
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        let state = self.state.borrow();
        let filter = state.filter.trim().to_lowercase();
        let current = state.current_id;

        let mut selected_row: Option<gtk::ListBoxRow> = None;

        for note in state.notes.iter() {
            if !filter.is_empty() && !note.matches(&filter) {
                continue;
            }
            let row = build_note_row(note);
            self.list_box.append(&row);
            if Some(note.id) == current {
                selected_row = Some(row);
            }
        }
        drop(state);

        if let Some(row) = selected_row {
            self.suppress.set(true);
            self.list_box.select_row(Some(&row));
            self.suppress.set(false);
        }
    }

    fn select_note(self: &Rc<Self>, id: i64) {
        if self.state.borrow().current_id == Some(id) {
            return;
        }
        // Save current before switching
        self.save_pending();

        let body = {
            let state = self.state.borrow();
            state
                .notes
                .iter()
                .find(|n| n.id == id)
                .map(|n| (n.body.clone(), n.updated_at))
        };

        let Some((body, updated_at)) = body else {
            return;
        };

        self.state.borrow_mut().current_id = Some(id);

        self.suppress.set(true);
        self.buffer.set_text(&body);
        self.suppress.set(false);

        self.update_title(&body, updated_at);
        self.update_placeholder();

        // Place cursor at end
        let end = self.buffer.end_iter();
        self.buffer.place_cursor(&end);

        // Highlight in list (without re-triggering selection signal blow-up)
        self.highlight_current_in_list();
    }

    fn highlight_current_in_list(&self) {
        let current = self.state.borrow().current_id;
        let Some(current) = current else { return };
        let mut child = self.list_box.first_child();
        while let Some(c) = child {
            if let Some(row) = c.downcast_ref::<gtk::ListBoxRow>() {
                let id_opt = unsafe { row.data::<i64>("note-id") };
                if let Some(ptr) = id_opt {
                    let id = unsafe { *ptr.as_ref() };
                    if id == current {
                        self.suppress.set(true);
                        self.list_box.select_row(Some(row));
                        self.suppress.set(false);
                        break;
                    }
                }
            }
            child = c.next_sibling();
        }
    }

    fn new_note(self: &Rc<Self>) {
        self.save_pending();
        match self.db.create_note() {
            Ok(note) => {
                let id = note.id;
                self.state.borrow_mut().notes.insert(0, note);
                self.rebuild_list();
                self.select_note(id);
                self.text_view.grab_focus();
            }
            Err(e) => tracing::error!("create note failed: {e}"),
        }
    }

    fn delete_current(self: &Rc<Self>) {
        let current = self.state.borrow().current_id;
        let Some(id) = current else { return };

        // Cancel autosave
        if let Some(handle) = self.autosave.borrow_mut().take() {
            handle.remove();
        }

        if let Err(e) = self.db.delete_note(id) {
            tracing::error!("delete failed: {e}");
            return;
        }

        let mut state = self.state.borrow_mut();
        state.notes.retain(|n| n.id != id);
        state.current_id = None;
        let next = state.notes.first().map(|n| n.id);
        drop(state);

        self.rebuild_list();
        if let Some(next_id) = next {
            self.select_note(next_id);
        } else {
            self.suppress.set(true);
            self.buffer.set_text("");
            self.suppress.set(false);
            self.title_label.set_text("");
            self.subtitle_label.set_text("");
            self.update_placeholder();
        }
    }

    fn schedule_autosave(self: &Rc<Self>) {
        // Debounce: cancel previous, schedule new
        if let Some(handle) = self.autosave.borrow_mut().take() {
            handle.remove();
        }
        let win = self.clone();
        let id = glib::timeout_add_local_once(
            std::time::Duration::from_millis(AUTOSAVE_DEBOUNCE_MS as u64),
            move || {
                win.autosave.borrow_mut().take();
                win.save_pending();
            },
        );
        *self.autosave.borrow_mut() = Some(id);
    }

    fn save_pending(&self) {
        let current = self.state.borrow().current_id;
        let Some(id) = current else { return };

        let body = self
            .buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), false)
            .to_string();
        let title = Note::derive_title(&body);

        if let Err(e) = self.db.update_note(id, &title, &body) {
            tracing::error!("save failed: {e}");
            return;
        }

        // Update in-memory state
        let now = Utc::now();
        let mut state = self.state.borrow_mut();
        if let Some(note) = state.notes.iter_mut().find(|n| n.id == id) {
            note.title = title.clone();
            note.body = body.clone();
            note.updated_at = now;
        }
        // Move to top
        if let Some(pos) = state.notes.iter().position(|n| n.id == id) {
            if pos > 0 {
                let n = state.notes.remove(pos);
                state.notes.insert(0, n);
            }
        }
        drop(state);

        self.update_title(&body, now);
        self.rebuild_list();
    }

    fn update_title(&self, body: &str, updated_at: DateTime<Utc>) {
        let title = Note::derive_title(body);
        if title.is_empty() {
            self.title_label.set_text("Untitled note");
        } else {
            self.title_label.set_text(&title);
        }
        let local: DateTime<Local> = updated_at.into();
        self.subtitle_label
            .set_text(&format!("Saved {}", local.format("%a %d %b · %H:%M")));
    }

    fn update_placeholder(&self) {
        let empty = self.buffer.char_count() == 0
            && self.state.borrow().current_id.is_some();
        let no_note = self.state.borrow().current_id.is_none();
        if no_note {
            self.placeholder
                .set_text("Pick a note or hit Ctrl+N to start");
            self.placeholder.set_visible(true);
        } else if empty {
            self.placeholder.set_text("Start typing…");
            self.placeholder.set_visible(true);
        } else {
            self.placeholder.set_visible(false);
        }
    }

    fn apply_opacity(&self) {
        let opacity = self.state.borrow().config.opacity;
        // The window itself stays fully opaque (so resize handles work),
        // but the inner stack uses CSS variable.
        // We achieve transparency by setting window.set_opacity for the whole surface.
        self.window.set_opacity(opacity);
    }

    fn apply_font_size(&self) {
        let size = self.state.borrow().config.font_size;
        let css = format!(
            ".jot-editor {{ font-size: {}px; }} .jot-editor text {{ font-size: {}px; }}",
            size, size
        );
        // Use a per-instance CSS provider so we can update it
        // Simplest: attach a new provider each time (cheap, GTK dedups)
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&css);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }
    }

    fn try_paste_image(self: &Rc<Self>) -> bool {
        let display = match gdk::Display::default() {
            Some(d) => d,
            None => return false,
        };
        let clipboard = display.clipboard();

        let formats = clipboard.formats();
        let has_image = formats.contains_type(gdk::Texture::static_type())
            || formats
                .mime_types()
                .iter()
                .any(|m: &glib::GString| m.starts_with("image/"));

        if !has_image {
            return false;
        }

        let win = self.clone();
        clipboard.read_texture_async(
            None::<&gio::Cancellable>,
            move |result| {
                if let Ok(Some(texture)) = result {
                    win.handle_pasted_texture(texture);
                }
            },
        );
        true
    }

    fn handle_pasted_texture(self: &Rc<Self>, texture: gdk::Texture) {
        // Save to disk, insert markdown link
        let images = match images_dir() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("images dir: {e}");
                return;
            }
        };
        let filename = format!("{}.png", uuid::Uuid::new_v4());
        let path: PathBuf = images.join(&filename);
        if let Err(e) = texture.save_to_png(&path) {
            tracing::error!("save image failed: {e}");
            return;
        }

        // Insert markdown reference at cursor
        let mark = self.buffer.get_insert();
        let mut iter = self.buffer.iter_at_mark(&mark);
        let text = format!("![image]({})", path.display());
        self.buffer.insert(&mut iter, &text);
    }
}

fn build_title(subtitle: &str) -> gtk::Box {
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let main = gtk::Label::new(Some("Jot"));
    main.add_css_class("jot-title");
    let sub = gtk::Label::new(Some(subtitle));
    sub.add_css_class("jot-subtitle");
    bx.append(&main);
    bx.append(&sub);
    bx
}

fn app_subtitle() -> String {
    "floating notes".to_string()
}

fn build_note_row(note: &Note) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let title_text = note.display_title();
    let title = gtk::Label::builder()
        .label(&title_text)
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .lines(1)
        .build();
    title.add_css_class("jot-row-title");

    let snippet_text = note.snippet();
    let snippet = gtk::Label::builder()
        .label(&snippet_text)
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .lines(1)
        .build();
    snippet.add_css_class("jot-row-snippet");

    let local: DateTime<Local> = note.updated_at.into();
    let time = gtk::Label::builder()
        .label(&format!("{}", local.format("%d %b · %H:%M")))
        .halign(gtk::Align::Start)
        .build();
    time.add_css_class("jot-row-time");

    bx.append(&title);
    if !snippet_text.is_empty() {
        bx.append(&snippet);
    }
    bx.append(&time);
    row.set_child(Some(&bx));

    let id = note.id;
    unsafe {
        row.set_data("note-id", id);
    }
    row
}
