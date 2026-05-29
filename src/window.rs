use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Local, Utc};
use gtk::glib;
use gtk::{gdk, gio};

use crate::config::{Config, Theme};
use crate::db::{images_dir, Db};
use crate::image_canvas::ImageCanvas;
use crate::maintenance;
use crate::markdown_tags::{refresh_markdown_tags, reveal_markers_at_cursor, MdTags};
use crate::note::Note;
use crate::themes::ThemeController;
use crate::transcribe::{TranscriptCmd, TranscriptEvent, TranscriptionHandle};

const AUTOSAVE_DEBOUNCE_MS: u32 = 400;
const UNDO_STACK_LIMIT: usize = 25;
const TEXT_UNDO_LIMIT: usize = 80;
const IMAGE_TAG_NAME: &str = "jot-image";
const COPY_TAG_NAME: &str = "jot-copy-source";
const URL_TAG_NAME: &str = "jot-url";
const IMAGE_MAX_W: i32 = 400;
const IMAGE_MAX_H: i32 = 260;
const DROP_TEXT_EXT: &[&str] = &["md", "markdown", "txt"];
const DROP_IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp"];
const NOTE_COLORS: &[(&str, &str)] = &[
    ("", "No color"),
    ("red", "Red"),
    ("orange", "Orange"),
    ("yellow", "Yellow"),
    ("green", "Green"),
    ("blue", "Blue"),
    ("purple", "Purple"),
    ("pink", "Pink"),
];

pub struct JotWindow {
    pub window: adw::ApplicationWindow,
    db: Db,
    list_box: gtk::ListBox,
    text_view: gtk::TextView,
    buffer: gtk::TextBuffer,
    image_tag: gtk::TextTag,
    copy_tag: gtk::TextTag,
    url_tag: gtk::TextTag,
    md_tags: MdTags,
    title_label: gtk::Label,
    subtitle_label: gtk::Label,
    placeholder: gtk::Label,
    tag_tabs: gtk::Box,
    note_meta: gtk::Box,
    note_meta_toggle: gtk::Button,
    tag_picker_button: gtk::MenuButton,
    tag_picker_label: gtk::Label,
    tag_picker_popover: gtk::Popover,
    tag_picker_search: gtk::SearchEntry,
    tag_picker_list: gtk::ListBox,
    tag_picker_actions: RefCell<Vec<TagPickerAction>>,
    color_buttons: RefCell<Vec<(String, gtk::ToggleButton)>>,
    search_entry: gtk::SearchEntry,
    mic_btn: gtk::Button,
    toast_overlay: adw::ToastOverlay,
    theme_controller: Rc<ThemeController>,
    state: RefCell<State>,
    suppress: Cell<bool>,
    autosave: RefCell<Option<glib::SourceId>>,
    pointer_pos: Cell<Option<(f64, f64)>>,
    ctrl_held: Cell<bool>,
    transcribe: RefCell<Option<TranscriptionHandle>>,
    /// State of the live voice session — pending chunks waiting for
    /// transcription, plus a TextMark anchored where text should land.
    voice_state: RefCell<Option<VoiceSession>>,
}

/// Tracks a single live voice-input session.
struct VoiceSession {
    /// Where in the buffer transcripts should be inserted. Has right
    /// gravity so the user can keep typing in front of incoming text.
    insert_mark: gtk::TextMark,
    /// Sequence number → received text (or `None` if still in flight).
    /// Insertion happens in seq order, so out-of-order responses queue.
    pending: std::collections::BTreeMap<u64, Option<String>>,
    /// Next seq number we expect to write into the buffer.
    next_to_emit: u64,
    /// True once Recording event arrived; UI can flip cursor / status.
    started: bool,
}

#[derive(Debug, Clone)]
enum TagPickerAction {
    Apply(String),
    Create(String),
}

struct State {
    notes: Vec<Note>,
    current_id: Option<i64>,
    config: Config,
    filter: String,
    active_tag: Option<String>,
    deleted_stack: Vec<Note>,
    text_undo: VecDeque<TextSnapshot>,
}

#[derive(Debug, Clone)]
struct TextSnapshot {
    note_id: i64,
    body: String,
}

impl JotWindow {
    pub fn build(app: &adw::Application) -> Rc<Self> {
        let db = Db::open().expect("could not open database");
        let config = Config::load();

        // ThemeController owns the single CssProvider for both the
        // palette and the opacity slider — they live together so the
        // light/dark switch can't be overridden by a stale opacity rule.
        let theme_controller = ThemeController::install(config.theme, config.opacity);

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

        let mic_btn = gtk::Button::builder()
            .icon_name("audio-input-microphone-symbolic")
            .tooltip_text("Voice transcription (Groq Whisper)")
            .build();
        mic_btn.add_css_class("jot-mic-btn");

        let record_btn = gtk::Button::builder()
            .icon_name("media-record-symbolic")
            .tooltip_text("Record screen GIF  ·  Super+R")
            .build();
        record_btn.add_css_class("jot-record-btn");

        let delete_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete current note  ·  Ctrl+D")
            .build();

        let export_btn = gtk::Button::builder()
            .icon_name("document-save-symbolic")
            .tooltip_text("Export current note as Markdown")
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
        header.pack_start(&mic_btn);
        header.pack_start(&record_btn);
        header.pack_end(&close_btn);
        header.pack_end(&settings_btn);
        header.pack_end(&delete_btn);
        header.pack_end(&export_btn);

        // Sidebar
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search notes…")
            .build();
        search_entry.add_css_class("jot-search");

        let tag_tabs = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tag_tabs.add_css_class("jot-tag-tabs");
        let tag_scroller = gtk::ScrolledWindow::builder()
            .child(&tag_tabs)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(38)
            .build();
        tag_scroller.add_css_class("jot-tag-tabs-scroll");
        // The horizontal overlay scrollbar sits on top of the tag tabs and
        // would intercept clicks even when hidden via CSS. Make it
        // non-targetable so clicks pass through to the tabs beneath.
        tag_scroller.hscrollbar().set_can_target(false);

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
        sidebar.append(&tag_scroller);
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
            .hexpand(true)
            .build();
        subtitle_label.add_css_class("jot-subtitle");

        let note_meta_toggle = gtk::Button::builder()
            .icon_name("pan-down-symbolic")
            .has_frame(false)
            .tooltip_text("Hide note tag and color controls")
            .build();
        note_meta_toggle.add_css_class("jot-meta-toggle");
        note_meta_toggle.set_cursor_from_name(Some("pointer"));

        let subtitle_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        subtitle_row.add_css_class("jot-subtitle-row");
        subtitle_row.append(&subtitle_label);
        subtitle_row.append(&note_meta_toggle);

        let tag_picker_label = gtk::Label::builder()
            .label("Add tag…")
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .build();
        tag_picker_label.add_css_class("jot-tag-picker-label");

        let tag_picker_icon = gtk::Image::from_icon_name("tag-outline-symbolic");
        tag_picker_icon.add_css_class("jot-tag-picker-icon");
        let tag_picker_chevron = gtk::Image::from_icon_name("pan-down-symbolic");
        tag_picker_chevron.add_css_class("jot-tag-picker-chevron");

        let tag_picker_button_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        tag_picker_button_content.append(&tag_picker_icon);
        tag_picker_button_content.append(&tag_picker_label);
        tag_picker_button_content.append(&tag_picker_chevron);

        let tag_picker_button = gtk::MenuButton::builder()
            .child(&tag_picker_button_content)
            .sensitive(false)
            .build();
        tag_picker_button.add_css_class("jot-tag-picker");
        tag_picker_button.set_size_request(160, -1);

        let tag_picker_search = gtk::SearchEntry::builder()
            .placeholder_text("Search or create…")
            .build();
        tag_picker_search.add_css_class("jot-tag-picker-search");

        let tag_picker_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        tag_picker_list.add_css_class("jot-tag-picker-list");

        let tag_picker_scroll = gtk::ScrolledWindow::builder()
            .child(&tag_picker_list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .min_content_height(60)
            .max_content_height(260)
            .propagate_natural_height(true)
            .build();
        tag_picker_scroll.add_css_class("jot-tag-picker-scroll");

        let tag_picker_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        tag_picker_box.set_margin_start(6);
        tag_picker_box.set_margin_end(6);
        tag_picker_box.set_margin_top(6);
        tag_picker_box.set_margin_bottom(6);
        tag_picker_box.append(&tag_picker_search);
        tag_picker_box.append(&tag_picker_scroll);

        let tag_picker_popover = gtk::Popover::builder()
            .child(&tag_picker_box)
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .build();
        tag_picker_popover.add_css_class("jot-tag-picker-popover");
        tag_picker_popover.set_size_request(260, -1);
        tag_picker_button.set_popover(Some(&tag_picker_popover));

        let color_palette = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        color_palette.add_css_class("jot-color-palette");
        let mut color_buttons = Vec::new();
        for (color, label) in NOTE_COLORS {
            let button = gtk::ToggleButton::builder()
                .has_frame(false)
                .tooltip_text(*label)
                .sensitive(false)
                .build();
            button.add_css_class("jot-color-swatch");
            if color.is_empty() {
                button.add_css_class("jot-color-default");
            } else {
                button.add_css_class(&format!("jot-color-{color}"));
            }
            color_palette.append(&button);
            color_buttons.push(((*color).to_string(), button));
        }

        let note_meta = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        note_meta.add_css_class("jot-note-meta");
        note_meta.append(&tag_picker_button);
        note_meta.append(&color_palette);

        let title_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        title_box.set_margin_start(20);
        title_box.set_margin_top(10);
        title_box.set_margin_bottom(2);
        title_box.append(&title_label);
        title_box.append(&subtitle_row);
        title_box.append(&note_meta);

        let buffer = gtk::TextBuffer::new(None);
        let image_tag = gtk::TextTag::builder()
            .name(IMAGE_TAG_NAME)
            .invisible(true)
            .editable(false)
            .build();
        buffer.tag_table().add(&image_tag);

        let copy_tag = gtk::TextTag::builder()
            .name(COPY_TAG_NAME)
            .invisible(true)
            .editable(false)
            .build();
        buffer.tag_table().add(&copy_tag);

        let url_tag = gtk::TextTag::builder()
            .name(URL_TAG_NAME)
            .underline(gtk::pango::Underline::Single)
            .foreground("#7c8cff")
            .build();
        buffer.tag_table().add(&url_tag);

        let md_tags = MdTags::install(&buffer);

        // Cursor-aware marker reveal: when the insert mark moves to a new
        // line, hide markers everywhere and re-show them only on that line.
        // The cell throttles work to actual line changes.
        let mh = md_tags.marker_hidden.clone();
        let mv = md_tags.marker_visible.clone();
        let last_line = std::rc::Rc::new(std::cell::Cell::new(-1i32));
        buffer.connect_mark_set(move |buf, _iter, mark| {
            if mark.name().as_deref() != Some("insert") {
                return;
            }
            let cursor = buf.iter_at_mark(&buf.get_insert());
            let line = cursor.line();
            if last_line.get() == line {
                return;
            }
            last_line.set(line);
            reveal_markers_at_cursor(buf, &mh, &mv);
        });

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

        // ToastOverlay sits between the split and the root so toasts hover
        // above the editor without affecting layout.
        let toast_overlay = adw::ToastOverlay::new();
        toast_overlay.set_child(Some(&split));

        // Root layout
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&toast_overlay);
        window.set_content(Some(&root));

        let this = Rc::new(JotWindow {
            window: window.clone(),
            db,
            list_box: list_box.clone(),
            text_view: text_view.clone(),
            buffer: buffer.clone(),
            image_tag,
            copy_tag,
            url_tag,
            md_tags,
            title_label,
            subtitle_label,
            placeholder,
            tag_tabs: tag_tabs.clone(),
            note_meta: note_meta.clone(),
            note_meta_toggle: note_meta_toggle.clone(),
            tag_picker_button: tag_picker_button.clone(),
            tag_picker_label: tag_picker_label.clone(),
            tag_picker_popover: tag_picker_popover.clone(),
            tag_picker_search: tag_picker_search.clone(),
            tag_picker_list: tag_picker_list.clone(),
            tag_picker_actions: RefCell::new(Vec::new()),
            color_buttons: RefCell::new(color_buttons),
            search_entry: search_entry.clone(),
            mic_btn: mic_btn.clone(),
            toast_overlay: toast_overlay.clone(),
            theme_controller,
            state: RefCell::new(State {
                notes: Vec::new(),
                current_id: None,
                config,
                filter: String::new(),
                active_tag: None,
                deleted_stack: Vec::new(),
                text_undo: VecDeque::new(),
            }),
            suppress: Cell::new(false),
            autosave: RefCell::new(None),
            pointer_pos: Cell::new(None),
            ctrl_held: Cell::new(false),
            transcribe: RefCell::new(None),
            voice_state: RefCell::new(None),
        });

        // Wire callbacks
        this.connect_callbacks(&new_btn, &delete_btn, &settings_btn, &close_btn);
        this.connect_metadata_controls();
        this.apply_note_meta_visibility();
        let win = this.clone();
        export_btn.connect_clicked(move |_| win.export_current_note());

        // GIF recorder — share the running adw::Application via the
        // window's application() handle, hand the main window over so
        // the recorder hides it while slurp owns the screen.
        let win = this.clone();
        record_btn.connect_clicked(move |_| win.launch_gif_recorder());
        this.install_shortcuts();
        this.install_url_click();
        this.install_url_hover_cursor();
        this.install_drop_target();
        this.apply_opacity();
        this.refresh_notes();
        this.update_mic_button_state();

        // If there are notes, select the first; otherwise show placeholder
        let first_id = {
            let state = this.state.borrow();
            state.notes.first().map(|n| n.id)
        };
        if let Some(first) = first_id {
            this.select_note(first);
        } else {
            this.update_note_metadata_controls();
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

        // Close request (window X, compositor "close window" / Hyprland
        // Super+W) hides instead of quitting. Jot is a resident, toggle-style
        // app — quitting the process would drop the Wayland clipboard offer,
        // so text copied from Jot couldn't be pasted anywhere once the window
        // closed. Matches Esc and the header close button, which already hide.
        let win = this.clone();
        window.connect_close_request(move |_| {
            win.save_pending();
            let _ = win.state.borrow().config.save();
            win.window.set_visible(false);
            glib::Propagation::Stop
        });

        // Sweep orphan images once the UI has settled. `idle_add_local_once`
        // fires when GTK has nothing higher-priority to do, so startup feel
        // stays instant even if the images dir is large.
        let db_for_vacuum = this.db.clone();
        glib::idle_add_local_once(move || {
            if let Err(e) = maintenance::vacuum_orphan_images(&db_for_vacuum) {
                tracing::warn!("image vacuum failed: {e}");
            }
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

        // Mic — toggles voice transcription.
        // Note: the actual `mic_btn` we wire here is the cloned reference
        // owned by `self`; the parameter argument is only used for layout.
        let win = self.clone();
        self.mic_btn.connect_clicked(move |_| win.toggle_voice());

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

    fn connect_metadata_controls(self: &Rc<Self>) {
        let win = self.clone();
        self.note_meta_toggle
            .connect_clicked(move |_| win.toggle_note_meta());

        let win = self.clone();
        self.tag_picker_popover.connect_show(move |_| {
            win.tag_picker_search.set_text("");
            win.refresh_tag_picker_list();
            win.tag_picker_search.grab_focus();
        });

        let win = self.clone();
        self.tag_picker_search.connect_search_changed(move |_| {
            win.refresh_tag_picker_list();
        });

        let win = self.clone();
        self.tag_picker_search.connect_activate(move |_| {
            win.activate_selected_tag_picker_row();
        });

        let win = self.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_, key, _, _| {
            match key {
                gdk::Key::Down => {
                    win.move_tag_picker_selection(1);
                    glib::Propagation::Stop
                }
                gdk::Key::Up => {
                    win.move_tag_picker_selection(-1);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.tag_picker_search.add_controller(key_ctrl);

        let win = self.clone();
        self.tag_picker_list
            .connect_row_activated(move |_, row| {
                let idx = row.index() as usize;
                let action = win.tag_picker_actions.borrow().get(idx).cloned();
                if let Some(action) = action {
                    win.apply_tag_picker_action(action);
                }
            });

        let color_buttons = self.color_buttons.borrow().clone();
        for (color, button) in color_buttons {
            let win = self.clone();
            button.connect_clicked(move |_| {
                if win.suppress.get() {
                    return;
                }
                win.set_current_color(&color);
            });
        }
    }

    /// Backspace/Delete pressed adjacent to an embedded block: drop the whole
    /// block — anchor + the invisible source run that follows it — in a
    /// single buffer.delete(). Without this, the hidden source tag's
    /// `editable=false` makes the user's Backspace silently no-op against
    /// the source text after they delete the surrounding text. Returns
    /// whether we consumed the keypress.
    fn try_delete_adjacent_embed(&self, backward: bool) -> bool {
        // Selection-delete is handled by GTK's default with our editable=false
        // tag, which we don't want to interfere with here.
        if self.buffer.has_selection() {
            return false;
        }

        let mark = self.buffer.get_insert();
        let cursor = self.buffer.iter_at_mark(&mark);

        // The probe is the char that the keypress would consume.
        let probe = if backward {
            let mut p = cursor;
            if !p.backward_char() {
                return false;
            }
            p
        } else {
            cursor
        };

        // Find the anchor position. Either:
        //  - probe is sitting on the U+FFFC anchor itself, or
        //  - probe is inside the invisible source run that trails an anchor.
        let anchor_iter = if probe.char() == '\u{FFFC}' {
            probe
        } else if probe.has_tag(&self.image_tag) || probe.has_tag(&self.copy_tag) {
            let mut walker = probe;
            while walker.char() != '\u{FFFC}' {
                if !walker.backward_char() {
                    return false;
                }
            }
            walker
        } else {
            return false;
        };

        let mut start = anchor_iter;
        let mut end = start;
        end.forward_char(); // past the anchor itself
        while end.has_tag(&self.image_tag) || end.has_tag(&self.copy_tag) {
            if !end.forward_char() {
                break;
            }
        }
        self.buffer.delete(&mut start, &mut end);
        true
    }

    fn install_shortcuts(self: &Rc<Self>) {
        // Window-level shortcuts (Bubble phase is fine — these don't conflict
        // with widget-level handling).
        let controller = gtk::EventControllerKey::new();
        let win = self.clone();
        controller.connect_key_pressed(move |_, keyval, _, modifier| {
            let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = modifier.contains(gdk::ModifierType::SHIFT_MASK);
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
                // Ctrl+Z: layered undo —
                //   1) restore last deleted note if any,
                //   2) otherwise roll the current note's body back to the
                //      previous snapshot (covers image deletions etc. that
                //      the TextView's native undo can't recreate),
                //   3) only fall through to native undo when both stacks
                //      are empty.
                gdk::Key::z if ctrl && !shift => {
                    if !win.state.borrow().deleted_stack.is_empty() {
                        win.undo_delete();
                        glib::Propagation::Stop
                    } else if win.try_text_undo() {
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(controller);

        // Paste handler — must run BEFORE the focused widget's default Ctrl+V.
        // We register on the WINDOW with Capture phase so it fires regardless
        // of which widget currently has focus (TextView, search entry, etc.)
        // and runs before the default paste-clipboard action.
        let paste_ctl = gtk::EventControllerKey::new();
        paste_ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let win = self.clone();
        paste_ctl.connect_key_pressed(move |_, keyval, _, modifier| {
            let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
            if ctrl && keyval == gdk::Key::v {
                tracing::debug!("Ctrl+V captured on window");
                if win.try_paste_image() {
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(paste_ctl);

        // Backspace/Delete inside the editor: intercept before the TextView's
        // default handler so we can atomically remove an inline image/copy block
        // when the cursor is touching one (anchor + invisible source).
        let edit_ctl = gtk::EventControllerKey::new();
        edit_ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let win = self.clone();
        edit_ctl.connect_key_pressed(move |_, keyval, _, modifier| {
            // Skip when the user is composing or holding modifiers we don't
            // recognise — let GTK do its thing.
            let plain = !modifier.contains(gdk::ModifierType::CONTROL_MASK)
                && !modifier.contains(gdk::ModifierType::ALT_MASK);
            if !plain {
                return glib::Propagation::Proceed;
            }
            match keyval {
                gdk::Key::BackSpace => {
                    if win.try_delete_adjacent_embed(true) {
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
                gdk::Key::Delete => {
                    if win.try_delete_adjacent_embed(false) {
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.text_view.add_controller(edit_ctl);
    }

    fn build_settings_popover(self: &Rc<Self>) -> gtk::Popover {
        let popover = gtk::Popover::new();
        popover.add_css_class("jot-settings");

        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
        stack.set_hexpand(true);

        // ── Appearance tab ──────────────────────────────────────────────
        let appearance = gtk::Box::new(gtk::Orientation::Vertical, 8);

        let theme_label = gtk::Label::builder()
            .label("Theme")
            .halign(gtk::Align::Start)
            .build();
        let theme_options = gtk::StringList::new(&["System", "Light", "Dark"]);
        let theme_dropdown = gtk::DropDown::builder().model(&theme_options).build();
        let initial_theme_idx = match self.state.borrow().config.theme {
            Theme::System => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        };
        theme_dropdown.set_selected(initial_theme_idx as u32);

        let win = self.clone();
        theme_dropdown.connect_selected_notify(move |dd| {
            let theme = match dd.selected() {
                1 => Theme::Light,
                2 => Theme::Dark,
                _ => Theme::System,
            };
            win.state.borrow_mut().config.theme = theme;
            win.theme_controller.apply(theme);
        });

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

        appearance.append(&theme_label);
        appearance.append(&theme_dropdown);
        appearance.append(&opacity_label);
        appearance.append(&opacity_scale);
        appearance.append(&font_label);
        appearance.append(&font_scale);
        appearance.append(&hint);

        // ── Voice tab ──────────────────────────────────────────────────
        let voice = gtk::Box::new(gtk::Orientation::Vertical, 8);

        let voice_title = gtk::Label::builder()
            .label("Groq Whisper Large v3 Turbo")
            .halign(gtk::Align::Start)
            .build();
        voice_title.add_css_class("jot-title");

        let voice_blurb = gtk::Label::builder()
            .label("Click the mic in the header to dictate. Speech is split at natural pauses and transcribed by Whisper on Groq (~216× real-time).")
            .halign(gtk::Align::Start)
            .wrap(true)
            .max_width_chars(38)
            .build();
        voice_blurb.add_css_class("jot-row-snippet");

        let key_label = gtk::Label::builder()
            .label("Groq API key")
            .halign(gtk::Align::Start)
            .build();
        let key_entry = gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .placeholder_text("paste your Groq API key (gsk_…)")
            .build();
        key_entry.set_text(&self.state.borrow().config.groq_api_key);
        key_entry.add_css_class("jot-search");
        key_entry.set_hexpand(true);

        let save_btn = gtk::Button::builder()
            .label("Save")
            .tooltip_text("Save and persist to ~/.config/jot/config.toml")
            .build();
        save_btn.add_css_class("jot-accent");

        let key_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        key_row.append(&key_entry);
        key_row.append(&save_btn);

        let key_link = gtk::LinkButton::builder()
            .label("Get one at console.groq.com/keys")
            .uri("https://console.groq.com/keys")
            .halign(gtk::Align::Start)
            .build();
        key_link.add_css_class("jot-row-time");

        // Optional language hint — empty string means "let Whisper auto-detect".
        let lang_label = gtk::Label::builder()
            .label("Language hint (ISO-639-1, blank = auto)")
            .halign(gtk::Align::Start)
            .build();
        let lang_entry = gtk::Entry::builder()
            .placeholder_text("pt, en, es … (blank for auto)")
            .max_length(8)
            .build();
        lang_entry.set_text(&self.state.borrow().config.transcribe_language);
        lang_entry.add_css_class("jot-search");

        let key_note = gtk::Label::builder()
            .label("Stored in plain text at ~/.config/jot/config.toml.")
            .halign(gtk::Align::Start)
            .wrap(true)
            .max_width_chars(38)
            .build();
        key_note.add_css_class("jot-row-time");

        // Live state update on every keystroke (so the mic button enables
        // immediately as you paste). Disk persistence happens on Save.
        let win = self.clone();
        key_entry.connect_changed(move |entry| {
            let v = entry.text().to_string();
            win.state.borrow_mut().config.groq_api_key = v;
            win.update_mic_button_state();
        });
        let win = self.clone();
        lang_entry.connect_changed(move |entry| {
            let v = entry.text().to_string();
            win.state.borrow_mut().config.transcribe_language = v;
        });

        let win = self.clone();
        let key_entry_for_save = key_entry.clone();
        let lang_entry_for_save = lang_entry.clone();
        save_btn.connect_clicked(move |_| {
            {
                let mut state = win.state.borrow_mut();
                state.config.groq_api_key = key_entry_for_save.text().to_string();
                state.config.transcribe_language = lang_entry_for_save.text().to_string();
            }
            let result = win.state.borrow().config.save();
            let toast = match result {
                Ok(()) => adw::Toast::builder()
                    .title("Voice settings saved")
                    .timeout(2)
                    .build(),
                Err(e) => adw::Toast::builder()
                    .title(format!("Could not save: {e}"))
                    .timeout(5)
                    .build(),
            };
            win.toast_overlay.add_toast(toast);
            win.update_mic_button_state();
        });

        voice.append(&voice_title);
        voice.append(&voice_blurb);
        voice.append(&key_label);
        voice.append(&key_row);
        voice.append(&key_link);
        voice.append(&lang_label);
        voice.append(&lang_entry);
        voice.append(&key_note);

        // ── Recorder tab ────────────────────────────────────────────────
        let recorder = gtk::Box::new(gtk::Orientation::Vertical, 8);

        let rec_title = gtk::Label::builder()
            .label("GIF recorder")
            .halign(gtk::Align::Start)
            .build();
        rec_title.add_css_class("jot-title");

        let rec_blurb = gtk::Label::builder()
            .label("Settings for the screen-region recorder (Super+R). Saved alongside the rest of your preferences.")
            .halign(gtk::Align::Start)
            .wrap(true)
            .max_width_chars(38)
            .build();
        rec_blurb.add_css_class("jot-row-snippet");

        let fps_label = gtk::Label::builder()
            .label("Frame rate")
            .halign(gtk::Align::Start)
            .build();
        let fps_options = gtk::StringList::new(&["12 fps", "15 fps", "20 fps", "24 fps", "30 fps"]);
        let fps_dropdown = gtk::DropDown::builder().model(&fps_options).build();
        let initial_fps_idx = match self.state.borrow().config.gif_fps {
            12 => 0,
            15 => 1,
            20 => 2,
            30 => 4,
            _ => 3, // default 24
        };
        fps_dropdown.set_selected(initial_fps_idx as u32);

        let win = self.clone();
        fps_dropdown.connect_selected_notify(move |dd| {
            let fps = match dd.selected() {
                0 => 12,
                1 => 15,
                2 => 20,
                4 => 30,
                _ => 24,
            };
            win.state.borrow_mut().config.gif_fps = fps;
        });

        let quality_label = gtk::Label::builder()
            .label("Quality preset")
            .halign(gtk::Align::Start)
            .build();
        let quality_options = gtk::StringList::new(&[
            "High — best motion, largest file",
            "Balanced — default",
            "Compact — small, may show banding",
        ]);
        let quality_dropdown = gtk::DropDown::builder().model(&quality_options).build();
        let initial_q_idx = match self.state.borrow().config.gif_quality {
            crate::config::GifQuality::High => 0,
            crate::config::GifQuality::Balanced => 1,
            crate::config::GifQuality::Compact => 2,
        };
        quality_dropdown.set_selected(initial_q_idx as u32);

        let win = self.clone();
        quality_dropdown.connect_selected_notify(move |dd| {
            let q = match dd.selected() {
                0 => crate::config::GifQuality::High,
                2 => crate::config::GifQuality::Compact,
                _ => crate::config::GifQuality::Balanced,
            };
            win.state.borrow_mut().config.gif_quality = q;
        });

        let rec_save_btn = gtk::Button::builder()
            .label("Save")
            .tooltip_text("Persist to ~/.config/jot/config.toml")
            .build();
        rec_save_btn.add_css_class("jot-accent");
        let win = self.clone();
        rec_save_btn.connect_clicked(move |_| {
            let result = win.state.borrow().config.save();
            let toast = match result {
                Ok(()) => adw::Toast::builder()
                    .title("Recorder settings saved")
                    .timeout(2)
                    .build(),
                Err(e) => adw::Toast::builder()
                    .title(format!("Could not save: {e}"))
                    .timeout(5)
                    .build(),
            };
            win.toast_overlay.add_toast(toast);
        });

        let rec_note = gtk::Label::builder()
            .label("Higher fps + High quality on long clips produces 10+ MB GIFs.")
            .halign(gtk::Align::Start)
            .wrap(true)
            .max_width_chars(38)
            .build();
        rec_note.add_css_class("jot-row-time");

        recorder.append(&rec_title);
        recorder.append(&rec_blurb);
        recorder.append(&fps_label);
        recorder.append(&fps_dropdown);
        recorder.append(&quality_label);
        recorder.append(&quality_dropdown);
        recorder.append(&rec_save_btn);
        recorder.append(&rec_note);

        // ── Data tab ────────────────────────────────────────────────────
        let data = gtk::Box::new(gtk::Orientation::Vertical, 8);

        let export_title = gtk::Label::builder()
            .label("Export")
            .halign(gtk::Align::Start)
            .build();
        export_title.add_css_class("jot-title");

        let export_blurb = gtk::Label::builder()
            .label("Export your notes as a folder of Markdown files. Inline images are bundled into a sibling images/ directory and references rewritten to relative paths.")
            .halign(gtk::Align::Start)
            .wrap(true)
            .max_width_chars(38)
            .build();
        export_blurb.add_css_class("jot-row-snippet");

        let export_all_btn = gtk::Button::builder().label("Export all notes…").build();
        export_all_btn.add_css_class("jot-accent");
        let win = self.clone();
        export_all_btn.connect_clicked(move |_| win.export_all_notes());

        data.append(&export_title);
        data.append(&export_blurb);
        data.append(&export_all_btn);

        stack.add_titled(&appearance, Some("appearance"), "Appearance");
        stack.add_titled(&voice, Some("voice"), "Voice");
        stack.add_titled(&recorder, Some("recorder"), "Recorder");
        stack.add_titled(&data, Some("data"), "Data");

        let switcher = gtk::StackSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.set_halign(gtk::Align::Center);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 10);
        container.append(&switcher);
        container.append(&stack);

        popover.set_child(Some(&container));
        popover
    }

    fn refresh_notes(self: &Rc<Self>) {
        let notes = self.db.list_notes().unwrap_or_default();
        self.state.borrow_mut().notes = notes;
        self.rebuild_tag_tabs();
        self.rebuild_list();
    }

    fn rebuild_tag_tabs(self: &Rc<Self>) {
        while let Some(child) = self.tag_tabs.first_child() {
            self.tag_tabs.remove(&child);
        }

        let (tags, has_untagged, active_tag) = {
            let mut state = self.state.borrow_mut();
            let (tags, has_untagged) = collect_note_tags(&state.notes);
            if let Some(active) = state.active_tag.as_deref() {
                let active_exists = if active.is_empty() {
                    has_untagged
                } else {
                    tags.iter().any(|t| t.eq_ignore_ascii_case(active))
                };
                if !active_exists {
                    state.active_tag = None;
                }
            }
            (tags, has_untagged, state.active_tag.clone())
        };

        let all = self.build_tag_tab("All", None, active_tag.is_none());
        self.tag_tabs.append(&all);

        if has_untagged {
            let active = active_tag.as_deref() == Some("");
            let untagged = self.build_tag_tab("Untagged", Some(String::new()), active);
            self.tag_tabs.append(&untagged);
        }

        for tag in tags {
            let active = active_tag
                .as_deref()
                .is_some_and(|current| current.eq_ignore_ascii_case(&tag));
            let tab = self.build_tag_tab(&tag, Some(tag.clone()), active);
            self.tag_tabs.append(&tab);
        }
    }

    fn build_tag_tab(
        self: &Rc<Self>,
        label: &str,
        tag_filter: Option<String>,
        active: bool,
    ) -> gtk::Button {
        let button = gtk::Button::builder().label(label).has_frame(false).build();
        button.add_css_class("jot-tag-tab");
        if active {
            button.add_css_class("jot-tag-tab-active");
        }

        let win = self.clone();
        button.connect_clicked(move |_| {
            win.state.borrow_mut().active_tag = tag_filter.clone();
            win.rebuild_tag_tabs();
            win.rebuild_list();
        });
        button
    }

    fn rebuild_list(self: &Rc<Self>) {
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        let state = self.state.borrow();
        let filter = state.filter.trim().to_lowercase();
        let active_tag = state.active_tag.clone();
        let current = state.current_id;

        let mut selected_row: Option<gtk::ListBoxRow> = None;

        let visible_notes: Vec<Note> = state
            .notes
            .iter()
            .filter(|n| note_matches_tag_filter(n, active_tag.as_deref()))
            .filter(|n| filter.is_empty() || n.matches(&filter))
            .cloned()
            .collect();
        drop(state);

        for note in &visible_notes {
            let row = self.build_note_row(note);
            self.list_box.append(&row);
            if Some(note.id) == current {
                selected_row = Some(row);
            }
        }

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
        // Switching notes mid-transcription would dump tokens into the
        // wrong place. Finalise (drops tentatives, clears handle) before
        // we move.
        if self.voice_active() {
            self.voice_stop();
            self.voice_finalise(None);
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
        self.load_body_into_buffer(&body);
        self.suppress.set(false);

        // Push the initial body so Ctrl+Z can roll back to "as freshly opened".
        self.push_text_snapshot(id, &body);

        self.update_title(&body, updated_at);
        self.update_note_metadata_controls();
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

        // Snapshot the note (with current buffer text, so unsaved edits survive undo)
        let live_body = self.extract_body_for_save();
        let snapshot: Option<Note> = {
            let state = self.state.borrow();
            state
                .notes
                .iter()
                .find(|n| n.id == id)
                .cloned()
                .map(|mut n| {
                    n.title = Note::derive_title(&live_body);
                    n.body = live_body;
                    n
                })
        };

        if let Err(e) = self.db.delete_note(id) {
            tracing::error!("delete failed: {e}");
            return;
        }

        let mut state = self.state.borrow_mut();
        state.notes.retain(|n| n.id != id);
        state.current_id = None;
        if let Some(note) = snapshot {
            state.deleted_stack.push(note);
            if state.deleted_stack.len() > UNDO_STACK_LIMIT {
                state.deleted_stack.remove(0);
            }
        }
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
            self.update_note_metadata_controls();
            self.update_placeholder();
        }

        // Surface a toast so the user knows the undo is one keystroke / one
        // click away. The toast button calls back into undo_delete which
        // pops from the same stack.
        let toast = adw::Toast::builder()
            .title("Note deleted")
            .button_label("Undo")
            .timeout(6)
            .build();
        let win = self.clone();
        toast.connect_button_clicked(move |_| win.undo_delete());
        self.toast_overlay.add_toast(toast);
    }

    fn undo_delete(self: &Rc<Self>) {
        let note = match self.state.borrow_mut().deleted_stack.pop() {
            Some(n) => n,
            None => return,
        };

        if let Err(e) = self.db.restore_note(&note) {
            tracing::error!("restore failed: {e}");
            self.state.borrow_mut().deleted_stack.push(note);
            return;
        }

        let id = note.id;
        {
            let mut state = self.state.borrow_mut();
            state.notes.push(note);
            state.notes.sort_by_key(|n| std::cmp::Reverse(n.updated_at));
        }
        self.rebuild_list();
        self.select_note(id);
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

    fn save_pending(self: &Rc<Self>) {
        let current = self.state.borrow().current_id;
        let Some(id) = current else { return };

        let body = self.extract_body_for_save();
        // Bail out when the buffer still matches what's stored for this note.
        // select_note() saves the outgoing note before switching, so without
        // this guard a no-op save would rewrite updated_at and bump the
        // untouched note to the top of the recency-sorted sidebar — making
        // the list reshuffle every time you click a different note.
        let unchanged = self
            .state
            .borrow()
            .notes
            .iter()
            .find(|n| n.id == id)
            .is_some_and(|n| n.body == body);
        if unchanged {
            return;
        }

        let title = Note::derive_title(&body);

        if let Err(e) = self.db.update_note(id, &title, &body) {
            tracing::error!("save failed: {e}");
            return;
        }

        // Update in-memory state
        let now = Utc::now();
        {
            let mut state = self.state.borrow_mut();
            if let Some(note) = state.notes.iter_mut().find(|n| n.id == id) {
                note.title = title.clone();
                note.body = body.clone();
                note.updated_at = now;
            }
            // Re-sort: pinned first, then by recency. A non-pinned note can
            // never jump above a pinned one even when it was just edited.
            state.notes.sort_by(|a, b| {
                b.pinned
                    .cmp(&a.pinned)
                    .then(b.updated_at.cmp(&a.updated_at))
            });
        }

        self.update_title(&body, now);
        self.rebuild_list();
        if !self.render_copy_blocks_if_needed(&body) {
            self.refresh_url_tags();
            refresh_markdown_tags(
                &self.buffer,
                &self.md_tags,
                &[&self.image_tag, &self.copy_tag],
            );
        }

        // Snapshot for Ctrl+Z. Each saved version becomes a step on the
        // text-undo stack.
        self.push_text_snapshot(id, &body);
    }

    fn push_text_snapshot(&self, note_id: i64, body: &str) {
        let mut state = self.state.borrow_mut();
        if state
            .text_undo
            .back()
            .is_some_and(|s| s.note_id == note_id && s.body == body)
        {
            return;
        }
        state.text_undo.push_back(TextSnapshot {
            note_id,
            body: body.to_string(),
        });
        while state.text_undo.len() > TEXT_UNDO_LIMIT {
            state.text_undo.pop_front();
        }
    }

    /// Pop the text-undo stack until we find a body that's actually
    /// different from what's in the buffer right now (skipping foreign
    /// notes and stale duplicates), then restore it. Returns whether a
    /// restore happened.
    fn try_text_undo(self: &Rc<Self>) -> bool {
        let current_id = match self.state.borrow().current_id {
            Some(id) => id,
            None => return false,
        };
        let current_body = self.extract_body_for_save();

        let target = loop {
            let mut state = self.state.borrow_mut();
            match state.text_undo.pop_back() {
                None => return false,
                Some(snap) if snap.note_id != current_id => continue,
                Some(snap) if snap.body == current_body => continue,
                Some(snap) => break snap,
            }
        };

        // Cancel any in-flight autosave so it doesn't immediately re-push
        // the pre-undo body and undo our undo.
        if let Some(handle) = self.autosave.borrow_mut().take() {
            handle.remove();
        }

        let title = Note::derive_title(&target.body);
        if let Err(e) = self.db.update_note(current_id, &title, &target.body) {
            tracing::error!("undo restore failed: {e}");
            return false;
        }

        self.suppress.set(true);
        self.load_body_into_buffer(&target.body);
        self.suppress.set(false);

        let now = Utc::now();
        {
            let mut state = self.state.borrow_mut();
            if let Some(note) = state.notes.iter_mut().find(|n| n.id == current_id) {
                note.title = title.clone();
                note.body = target.body.clone();
                note.updated_at = now;
            }
            state.notes.sort_by(|a, b| {
                b.pinned
                    .cmp(&a.pinned)
                    .then(b.updated_at.cmp(&a.updated_at))
            });
        }
        self.update_title(&target.body, now);
        self.rebuild_list();

        // Place cursor at end so user can continue typing where it makes sense.
        let end = self.buffer.end_iter();
        self.buffer.place_cursor(&end);
        true
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
        let empty = self.buffer.char_count() == 0 && self.state.borrow().current_id.is_some();
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
        // ThemeController owns the combined theme + opacity provider.
        // Forward the slider value so the active palette stays correct.
        self.theme_controller.set_opacity(opacity);
    }

    fn toggle_note_meta(self: &Rc<Self>) {
        let config = {
            let mut state = self.state.borrow_mut();
            state.config.note_meta_collapsed = !state.config.note_meta_collapsed;
            state.config.clone()
        };
        if let Err(e) = config.save() {
            tracing::warn!("save note metadata collapse state failed: {e}");
        }
        self.apply_note_meta_visibility();
    }

    fn apply_note_meta_visibility(&self) {
        let collapsed = self.state.borrow().config.note_meta_collapsed;
        self.note_meta.set_visible(!collapsed);
        if collapsed {
            self.note_meta_toggle.set_icon_name("pan-end-symbolic");
            self.note_meta_toggle
                .set_tooltip_text(Some("Show note tag and color controls"));
        } else {
            self.note_meta_toggle.set_icon_name("pan-down-symbolic");
            self.note_meta_toggle
                .set_tooltip_text(Some("Hide note tag and color controls"));
        }
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
        let Some(display) = gdk::Display::default() else {
            return false;
        };
        let clipboard = display.clipboard();
        let formats = clipboard.formats();

        let mimes: Vec<String> = formats.mime_types().iter().map(|m| m.to_string()).collect();
        let has_texture = formats.contains_type(gdk::Texture::static_type());
        let has_image_mime = mimes.iter().any(|m| m.starts_with("image/"));
        tracing::info!("Ctrl+V: texture={has_texture} image_mime={has_image_mime} mimes={mimes:?}");

        if !has_texture && !has_image_mime {
            return false;
        }

        // Snapshot which note is open RIGHT NOW; the read_texture_async
        // callback fires later and the user might switch notes in between.
        // Without this guard, the image lands in the wrong note.
        let target_id = self.state.borrow().current_id;
        let win = self.clone();
        clipboard.read_texture_async(None::<&gio::Cancellable>, move |result| match result {
            Ok(Some(texture)) => {
                let now_id = win.state.borrow().current_id;
                if now_id != target_id {
                    tracing::info!(
                        "paste cancelled: note switched ({:?} -> {:?})",
                        target_id,
                        now_id
                    );
                    return;
                }
                tracing::info!("received texture {}x{}", texture.width(), texture.height());
                win.handle_pasted_texture(texture);
            }
            Ok(None) => tracing::warn!("clipboard returned no texture"),
            Err(e) => tracing::warn!("read_texture_async failed: {e}"),
        });
        true
    }

    fn handle_pasted_texture(self: &Rc<Self>, texture: gdk::Texture) {
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
        tracing::info!("image saved to {}", path.display());
        self.insert_image_at_cursor(&path, &texture);
    }

    /// Load a markdown-style body into the buffer, replacing `![image](path)`
    /// references with inline Picture widgets while keeping the markdown text
    /// preserved under an invisible tag for round-trip persistence.
    fn load_body_into_buffer(self: &Rc<Self>, body: &str) {
        self.buffer.set_text("");
        for part in parse_markdown_body(body) {
            match part {
                BodyPart::Text(t) => {
                    let mut iter = self.buffer.end_iter();
                    self.buffer.insert(&mut iter, &t);
                }
                BodyPart::Image(path_str) => {
                    self.insert_image_part(&path_str);
                }
                BodyPart::CopyBlock(content) => {
                    self.insert_copy_block_part(&content);
                }
            }
        }
        self.refresh_url_tags();
        refresh_markdown_tags(
            &self.buffer,
            &self.md_tags,
            &[&self.image_tag, &self.copy_tag],
        );
    }

    fn render_copy_blocks_if_needed(self: &Rc<Self>, body: &str) -> bool {
        if !self.has_unrendered_copy_block() {
            return false;
        }

        let cursor_offset = self.buffer.iter_at_mark(&self.buffer.get_insert()).offset();
        self.suppress.set(true);
        self.load_body_into_buffer(body);
        self.suppress.set(false);

        let adjusted_offset = cursor_offset + copy_blocks_before_char_offset(body, cursor_offset);
        let end_offset = self.buffer.end_iter().offset();
        let cursor = self.buffer.iter_at_offset(adjusted_offset.min(end_offset));
        self.buffer.place_cursor(&cursor);
        self.update_placeholder();
        true
    }

    fn has_unrendered_copy_block(&self) -> bool {
        const COPY_OPEN: &str = "{copy}";
        const COPY_CLOSE: &str = "{/copy}";

        let start = self.buffer.start_iter();
        let end = self.buffer.end_iter();
        let text = self.buffer.text(&start, &end, true).to_string();
        let mut byte_pos = 0;

        while let Some(rel_open) = text[byte_pos..].find(COPY_OPEN) {
            let open = byte_pos + rel_open;
            let after_open = open + COPY_OPEN.len();
            let Some(rel_close) = text[after_open..].find(COPY_CLOSE) else {
                return false;
            };
            let char_offset = text[..open].chars().count() as i32;
            // Query inside the hidden source range, not exactly at its left
            // boundary. GTK tag state at a toggle boundary can read as off,
            // which would make autosave re-render every existing copy block
            // and yank the editor scroll position.
            let iter = self.buffer.iter_at_offset(char_offset + 1);
            if !iter.has_tag(&self.copy_tag) {
                return true;
            }
            byte_pos = after_open + rel_close + COPY_CLOSE.len();
        }

        false
    }

    fn insert_image_part(self: &Rc<Self>, path_str: &str) {
        let path = Path::new(path_str);
        match gdk::Texture::from_filename(path) {
            Ok(texture) => self.embed_image(path, &texture, /*with_newlines=*/ false),
            Err(e) => {
                tracing::warn!("could not load {}: {e}", path_str);
                let mut iter = self.buffer.end_iter();
                self.buffer
                    .insert(&mut iter, &format!("![image]({})", path_str));
            }
        }
    }

    fn insert_copy_block_part(self: &Rc<Self>, content: &str) {
        let mut iter = self.buffer.end_iter();
        let anchor = self.buffer.create_child_anchor(&mut iter);

        let payload = copy_block_payload(content);
        let block = gtk::Box::new(gtk::Orientation::Vertical, 6);
        block.add_css_class("jot-copy-block");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let title = gtk::Label::builder()
            .label("Copy")
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .build();
        title.add_css_class("jot-copy-title");
        title.set_hexpand(true);

        let copy_btn = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copy block to clipboard")
            .build();
        copy_btn.add_css_class("jot-copy-button");
        copy_btn.set_cursor_from_name(Some("pointer"));
        header.append(&title);
        header.append(&copy_btn);

        let display_payload = if payload.is_empty() {
            "(empty)".to_string()
        } else {
            payload.clone()
        };
        let text = gtk::Label::builder()
            .label(&display_payload)
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        text.add_css_class("jot-copy-text");

        block.append(&header);
        block.append(&text);
        self.text_view.add_child_at_anchor(&block, &anchor);

        let win = self.clone();
        let clipboard_text = payload.clone();
        copy_btn.connect_clicked(move |_| {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&clipboard_text);
                win.toast_overlay.add_toast(
                    adw::Toast::builder()
                        .title("Copied block to clipboard")
                        .timeout(2)
                        .build(),
                );
            } else {
                win.toast_overlay.add_toast(
                    adw::Toast::builder()
                        .title("Clipboard unavailable")
                        .timeout(3)
                        .build(),
                );
            }
        });

        let after_anchor = iter.offset();
        let mut raw_iter = self.buffer.iter_at_offset(after_anchor);
        let raw = format!("{{copy}}{content}{{/copy}}");
        self.buffer.insert(&mut raw_iter, &raw);
        let raw_chars = raw.chars().count() as i32;
        let start = self.buffer.iter_at_offset(after_anchor);
        let end = self.buffer.iter_at_offset(after_anchor + raw_chars);
        self.buffer.apply_tag(&self.copy_tag, &start, &end);

        let after_tag = self.buffer.iter_at_offset(after_anchor + raw_chars);
        self.buffer.place_cursor(&after_tag);
    }

    fn insert_image_at_cursor(self: &Rc<Self>, path: &Path, texture: &gdk::Texture) {
        // Drop a newline before the image if we're not on a fresh line, so
        // the image isn't squashed against existing text.
        let mark = self.buffer.get_insert();
        let iter = self.buffer.iter_at_mark(&mark);
        if !iter.starts_line() {
            let mut iter = self.buffer.iter_at_mark(&mark);
            self.buffer.insert(&mut iter, "\n");
        }
        self.embed_image_at_cursor(path, texture, /*with_newlines=*/ true);
    }

    /// Insert image at the END of the buffer (used during body load).
    fn embed_image(self: &Rc<Self>, path: &Path, texture: &gdk::Texture, with_newlines: bool) {
        let mut iter = self.buffer.end_iter();
        self.embed_image_at_iter(&mut iter, path, texture, with_newlines);
    }

    /// Insert image at the current insert mark (used by paste).
    fn embed_image_at_cursor(
        self: &Rc<Self>,
        path: &Path,
        texture: &gdk::Texture,
        with_newlines: bool,
    ) {
        let mark = self.buffer.get_insert();
        let mut iter = self.buffer.iter_at_mark(&mark);
        self.embed_image_at_iter(&mut iter, path, texture, with_newlines);
    }

    fn embed_image_at_iter(
        self: &Rc<Self>,
        iter: &mut gtk::TextIter,
        path: &Path,
        texture: &gdk::Texture,
        with_newlines: bool,
    ) {
        // 1. Anchor + Picture (visible inline)
        let anchor = self.buffer.create_child_anchor(iter);

        let (w, h) = scale_to_fit(texture.width(), texture.height(), IMAGE_MAX_W, IMAGE_MAX_H);
        let picture = gtk::Picture::for_paintable(texture);
        picture.add_css_class("jot-inline-image");
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_size_request(w, h);
        picture.set_halign(gtk::Align::Start);
        picture.set_valign(gtk::Align::Start);
        picture.set_cursor_from_name(Some("pointer"));
        picture.set_tooltip_text(Some("Click to open larger preview"));
        self.text_view.add_child_at_anchor(&picture, &anchor);

        // Click on the inline picture opens a preview dialog.
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let win = self.clone();
        let tex_for_preview = texture.clone();
        click.connect_pressed(move |gesture, n_press, _, _| {
            if n_press == 1 {
                win.show_image_preview(&tex_for_preview);
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        });
        picture.add_controller(click);

        // 2. Invisible markdown reference (so the body round-trips). After
        // creating the anchor, `iter` has advanced past it — re-fetch via
        // offset so the next insert lands right after the anchor.
        let after_anchor = iter.offset();
        let mut iter = self.buffer.iter_at_offset(after_anchor);
        let md = if with_newlines {
            format!("\n![image]({})\n", path.display())
        } else {
            format!("![image]({})", path.display())
        };
        self.buffer.insert(&mut iter, &md);
        let md_chars = md.chars().count() as i32;
        let start = self.buffer.iter_at_offset(after_anchor);
        let end = self.buffer.iter_at_offset(after_anchor + md_chars);
        self.buffer.apply_tag(&self.image_tag, &start, &end);

        // Place the cursor *past* the invisible markdown so a subsequent
        // keystroke lands in visible text, not in the hidden region (which
        // is editable=false and would silently swallow input).
        let after_tag = self.buffer.iter_at_offset(after_anchor + md_chars);
        self.buffer.place_cursor(&after_tag);
    }

    fn show_image_preview(self: &Rc<Self>, texture: &gdk::Texture) {
        let dialog = adw::Dialog::builder()
            .content_width(1000)
            .content_height(760)
            .can_close(true)
            .title("")
            .build();
        dialog.add_css_class("jot-image-preview");

        let canvas = ImageCanvas::new(texture);
        canvas.add_css_class("jot-canvas");

        // Header bar with zoom controls. Zoom buttons drive the same
        // `zoom_by` math as the scroll wheel, so they stay anchored at the
        // pointer's current position too.
        let title = gtk::Label::builder().label("Image preview").build();
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.add_css_class("jot-headerbar");

        let zoom_out_btn = gtk::Button::builder()
            .icon_name("zoom-out-symbolic")
            .tooltip_text("Zoom out  ·  Ctrl + scroll down")
            .build();
        let zoom_label = gtk::Label::builder()
            .label(format!("{:.0}%", canvas.zoom_pct()))
            .width_chars(5)
            .build();
        zoom_label.add_css_class("jot-row-time");
        let zoom_in_btn = gtk::Button::builder()
            .icon_name("zoom-in-symbolic")
            .tooltip_text("Zoom in  ·  Ctrl + scroll up")
            .build();
        let reset_btn = gtk::Button::builder()
            .icon_name("zoom-fit-best-symbolic")
            .tooltip_text("Reset view  ·  double-click")
            .build();

        let canvas_for_btn = canvas.clone();
        let lbl = zoom_label.clone();
        zoom_in_btn.connect_clicked(move |_| {
            canvas_for_btn.zoom_in();
            lbl.set_label(&format!("{:.0}%", canvas_for_btn.zoom_pct()));
        });
        let canvas_for_btn = canvas.clone();
        let lbl = zoom_label.clone();
        zoom_out_btn.connect_clicked(move |_| {
            canvas_for_btn.zoom_out();
            lbl.set_label(&format!("{:.0}%", canvas_for_btn.zoom_pct()));
        });
        let canvas_for_btn = canvas.clone();
        let lbl = zoom_label.clone();
        reset_btn.connect_clicked(move |_| {
            canvas_for_btn.reset_view();
            lbl.set_label(&format!("{:.0}%", canvas_for_btn.zoom_pct()));
        });

        // Keep the % label in sync with scroll-wheel zooms too.
        let lbl = zoom_label.clone();
        let canvas_for_tick = canvas.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
            lbl.set_label(&format!("{:.0}%", canvas_for_tick.zoom_pct()));
            glib::ControlFlow::Continue
        });

        header.pack_start(&zoom_out_btn);
        header.pack_start(&zoom_label);
        header.pack_start(&zoom_in_btn);
        header.pack_end(&reset_btn);

        let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bx.append(&header);
        bx.append(&canvas);

        dialog.set_child(Some(&bx));
        dialog.present(Some(&self.window));
    }

    /// Extract the body for persistence — `text()` with `include_hidden=true`
    /// returns the visible text plus the invisible markdown references. We
    /// strip the Object Replacement Character (U+FFFC) that GTK uses as the
    /// in-stream marker for child anchors / paintables.
    fn extract_body_for_save(&self) -> String {
        let raw = self
            .buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), true)
            .to_string();
        raw.replace('\u{FFFC}', "")
    }

    fn buffer_has_child_anchors(&self) -> bool {
        self.buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), true)
            .to_string()
            .contains('\u{FFFC}')
    }

    /// Strip then re-apply the URL underline tag across the whole buffer.
    /// Cheap enough to run on every load and after each autosave.
    fn refresh_url_tags(&self) {
        let start = self.buffer.start_iter();
        let end = self.buffer.end_iter();
        self.buffer.remove_tag(&self.url_tag, &start, &end);

        let text = self.buffer.text(&start, &end, true).to_string();

        for (off, len) in find_urls(&text) {
            let s = self.buffer.iter_at_offset(off as i32);
            // Skip URLs that fall inside an invisible image-markdown region —
            // those aren't really visible URLs the user can click; they're
            // just paths on disk that happen to start with a scheme.
            if s.has_tag(&self.image_tag) || s.has_tag(&self.copy_tag) {
                continue;
            }
            let e = self.buffer.iter_at_offset((off + len) as i32);
            self.buffer.apply_tag(&self.url_tag, &s, &e);
        }
    }

    fn open_url_at_iter(&self, iter: &gtk::TextIter) -> bool {
        if !iter.has_tag(&self.url_tag) {
            return false;
        }
        let mut start = *iter;
        if !start.starts_tag(Some(&self.url_tag)) {
            start.backward_to_tag_toggle(Some(&self.url_tag));
        }
        let mut end = *iter;
        end.forward_to_tag_toggle(Some(&self.url_tag));
        let url = self.buffer.text(&start, &end, false).to_string();
        if url.is_empty() {
            return false;
        }
        tracing::info!("launching url {url}");
        let launcher = gtk::UriLauncher::new(&url);
        launcher.launch(Some(&self.window), None::<&gio::Cancellable>, |result| {
            if let Err(e) = result {
                tracing::warn!("UriLauncher failed: {e}");
            }
        });
        true
    }

    fn install_url_click(self: &Rc<Self>) {
        let click_ctl = gtk::GestureClick::new();
        click_ctl.set_button(gdk::BUTTON_PRIMARY);
        let win = self.clone();
        let tv = self.text_view.clone();
        click_ctl.connect_pressed(move |gesture, n_press, x, y| {
            if n_press != 1 {
                return;
            }
            let event = match gesture.current_event() {
                Some(e) => e,
                None => return,
            };
            let mods = event.modifier_state();
            if !mods.contains(gdk::ModifierType::CONTROL_MASK) {
                return;
            }
            if win.buffer_has_child_anchors() {
                return;
            }
            let (bx, by) =
                tv.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            if let Some(iter) = tv.iter_at_location(bx, by) {
                if win.open_url_at_iter(&iter) {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            }
        });
        self.text_view.add_controller(click_ctl);
    }

    fn install_url_hover_cursor(self: &Rc<Self>) {
        // Motion: track pointer position over the editor and the live
        // modifier state so we can flip the cursor without waiting for a
        // separate keyboard event.
        let motion = gtk::EventControllerMotion::new();
        let win = self.clone();
        motion.connect_motion(move |controller, x, y| {
            win.pointer_pos.set(Some((x, y)));
            if let Some(event) = controller.current_event() {
                let ctrl = event
                    .modifier_state()
                    .contains(gdk::ModifierType::CONTROL_MASK);
                win.ctrl_held.set(ctrl);
            }
            win.refresh_link_cursor();
        });
        let win = self.clone();
        motion.connect_leave(move |_| {
            win.pointer_pos.set(None);
            win.refresh_link_cursor();
        });
        self.text_view.add_controller(motion);

        // Modifiers: catch Ctrl press / release even while the pointer is
        // stationary, so the cursor flips the moment the user holds Ctrl.
        let key = gtk::EventControllerKey::new();
        let win = self.clone();
        key.connect_modifiers(move |_, mods| {
            let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
            if ctrl != win.ctrl_held.get() {
                win.ctrl_held.set(ctrl);
                win.refresh_link_cursor();
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(key);
    }

    fn refresh_link_cursor(&self) {
        // Skip the iter-at-location query entirely when Ctrl isn't held.
        // It only matters for the "pointer" cursor decision, and the GTK
        // call has triggered crashes with certain buffer states (mix of
        // visible/invisible runs around child anchors). Limiting it to
        // safe buffers and the moments we actually care about avoids the
        // crash and saves a query on every mouse move.
        let want_pointer = self.ctrl_held.get()
            && match self.pointer_pos.get() {
                Some((x, y)) => self.is_pointer_over_url(x, y),
                None => false,
            };
        let name = if want_pointer { "pointer" } else { "text" };
        self.text_view.set_cursor_from_name(Some(name));
    }

    fn is_pointer_over_url(&self, x: f64, y: f64) -> bool {
        if self.buffer.char_count() == 0 {
            return false;
        }
        if self.buffer_has_child_anchors() {
            return false;
        }
        let (bx, by) =
            self.text_view
                .window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        if bx < 0 || by < 0 {
            return false;
        }
        match self.text_view.iter_at_location(bx, by) {
            Some(iter) => iter.has_tag(&self.url_tag),
            None => false,
        }
    }

    // ────────────────────────────── voice ─────────────────────────────

    fn voice_active(&self) -> bool {
        self.transcribe.borrow().is_some()
    }

    fn pending_chunk_count(&self) -> usize {
        self.voice_state
            .borrow()
            .as_ref()
            .map(|s| s.pending.values().filter(|v| v.is_none()).count())
            .unwrap_or(0)
    }

    fn update_mic_button_state(&self) {
        let has_key = !self.state.borrow().config.groq_api_key.trim().is_empty();
        let active = self.voice_active();
        let started = self
            .voice_state
            .borrow()
            .as_ref()
            .map(|s| s.started)
            .unwrap_or(false);
        self.mic_btn.set_sensitive(has_key);
        if active {
            self.mic_btn.add_css_class("jot-mic-recording");
            self.mic_btn.set_icon_name("media-playback-stop-symbolic");
            self.mic_btn.set_tooltip_text(Some("Stop transcription"));
            let pending = self.pending_chunk_count();
            let label = if !started {
                "● Connecting…".to_string()
            } else if pending > 0 {
                format!("● Recording  ·  {pending} chunk(s) in flight")
            } else {
                "● Recording".to_string()
            };
            self.subtitle_label.set_text(&label);
            self.subtitle_label.add_css_class("jot-mic-listening");
        } else {
            self.mic_btn.remove_css_class("jot-mic-recording");
            self.mic_btn
                .set_icon_name("audio-input-microphone-symbolic");
            self.mic_btn.set_tooltip_text(Some(if has_key {
                "Start voice transcription"
            } else {
                "Set your Groq API key in Settings → Voice"
            }));
            self.subtitle_label.remove_css_class("jot-mic-listening");
        }
    }

    fn toggle_voice(self: &Rc<Self>) {
        if self.voice_active() {
            self.voice_stop();
        } else {
            self.voice_start();
        }
    }

    fn voice_start(self: &Rc<Self>) {
        let api_key = self.state.borrow().config.groq_api_key.trim().to_string();
        if api_key.is_empty() {
            let toast = adw::Toast::builder()
                .title("Set your Groq API key in Settings → Voice first")
                .timeout(4)
                .build();
            self.toast_overlay.add_toast(toast);
            return;
        }
        if self.state.borrow().current_id.is_none() {
            self.new_note();
        }

        let language = {
            let l = self.state.borrow().config.transcribe_language.clone();
            let trimmed = l.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        };

        let (handle, evt_rx) = crate::transcribe::start(api_key, language);
        *self.transcribe.borrow_mut() = Some(handle);

        // Anchor where transcripts will land — at the current cursor, with
        // right gravity so the user can keep typing in front and the mark
        // moves with the text behind them.
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        // Make sure we start on a fresh line if the cursor is mid-sentence,
        // so the first chunk doesn't stitch into existing words.
        if !cursor.starts_line() {
            self.suppress.set(true);
            let mut iter = self.buffer.iter_at_mark(&self.buffer.get_insert());
            self.buffer.insert(&mut iter, "\n");
            self.suppress.set(false);
        }
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let insert_mark = self.buffer.create_mark(None, &cursor, false);

        *self.voice_state.borrow_mut() = Some(VoiceSession {
            insert_mark,
            pending: std::collections::BTreeMap::new(),
            next_to_emit: 0,
            started: false,
        });
        self.update_mic_button_state();

        let win = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(evt) = evt_rx.recv().await {
                win.handle_voice_event(evt);
            }
        });
    }

    fn voice_stop(&self) {
        if let Some(handle) = self.transcribe.borrow().as_ref() {
            let tx = handle.cmd_tx.clone();
            glib::spawn_future_local(async move {
                let _ = tx.send(TranscriptCmd::Stop).await;
            });
        }
    }

    fn handle_voice_event(self: &Rc<Self>, evt: TranscriptEvent) {
        match evt {
            TranscriptEvent::Recording => {
                if let Some(s) = self.voice_state.borrow_mut().as_mut() {
                    s.started = true;
                }
                self.update_mic_button_state();
                tracing::info!("voice: recording");
            }
            TranscriptEvent::ChunkPending { seq } => {
                if let Some(s) = self.voice_state.borrow_mut().as_mut() {
                    s.pending.entry(seq).or_insert(None);
                }
                self.update_mic_button_state();
            }
            TranscriptEvent::Chunk { seq, text } => {
                if let Some(s) = self.voice_state.borrow_mut().as_mut() {
                    s.pending.insert(seq, Some(text));
                }
                self.voice_drain_ready();
                self.update_mic_button_state();
            }
            TranscriptEvent::Finished => {
                self.voice_drain_ready();
                self.voice_finalise(None);
            }
            TranscriptEvent::Error(msg) => {
                tracing::warn!("voice: {msg}");
                self.voice_drain_ready();
                self.voice_finalise(Some(msg));
            }
        }
    }

    /// Pull every transcript that's ready in seq order and stitch it into
    /// the buffer at `insert_mark`. Stops at the first hole — keeps spoken
    /// order even when HTTP responses come back out of order.
    fn voice_drain_ready(self: &Rc<Self>) {
        loop {
            let ready_text = {
                let mut state = self.voice_state.borrow_mut();
                let Some(state) = state.as_mut() else { return };
                let next = state.next_to_emit;
                match state.pending.get(&next) {
                    Some(Some(_)) => {
                        let text = state.pending.remove(&next).unwrap().unwrap();
                        state.next_to_emit += 1;
                        text
                    }
                    _ => return,
                }
            };

            // Whisper sometimes returns leading whitespace; collapse it.
            let trimmed = ready_text.trim();
            if trimmed.is_empty() {
                continue;
            }

            let mark = match self.voice_state.borrow().as_ref() {
                Some(s) => s.insert_mark.clone(),
                None => return,
            };

            self.suppress.set(true);
            let mut iter = self.buffer.iter_at_mark(&mark);
            // Add a single space between consecutive utterances so they
            // don't run together, but skip the leading space at the very
            // start of the session.
            let need_separator = iter.offset() > 0
                && iter
                    .clone()
                    .backward_char()
                    .then(|| iter.char())
                    .map(|c| c != ' ' && c != '\n')
                    .unwrap_or(false);
            if need_separator {
                self.buffer.insert(&mut iter, " ");
            }
            self.buffer.insert(&mut iter, trimmed);
            self.suppress.set(false);

            self.update_placeholder();
            self.schedule_autosave();
        }
    }

    fn voice_finalise(self: &Rc<Self>, error: Option<String>) {
        if let Some(state) = self.voice_state.borrow_mut().take() {
            self.buffer.delete_mark(&state.insert_mark);
        }
        *self.transcribe.borrow_mut() = None;
        self.update_mic_button_state();
        // Restore the saved-at subtitle.
        if let Some(id) = self.state.borrow().current_id {
            if let Some(updated_at) = self
                .state
                .borrow()
                .notes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.updated_at)
            {
                let local: DateTime<Local> = updated_at.into();
                self.subtitle_label
                    .set_text(&format!("Saved {}", local.format("%a %d %b · %H:%M")));
            }
        }
        if let Some(msg) = error {
            let toast = adw::Toast::builder()
                .title(format!("Voice: {msg}"))
                .timeout(5)
                .build();
            self.toast_overlay.add_toast(toast);
        }
        self.save_pending();
    }

    // ────────────────────────────── export ────────────────────────────

    fn launch_gif_recorder(self: &Rc<Self>) {
        let Some(app) = self.window.application() else {
            tracing::warn!("no application — cannot start recorder");
            return;
        };
        let Ok(app) = app.downcast::<adw::Application>() else {
            tracing::warn!("application is not adw::Application");
            return;
        };
        crate::recorder_window::launch_recorder(&app, Some(self.window.clone()));
    }

    fn export_current_note(self: &Rc<Self>) {
        let Some(id) = self.state.borrow().current_id else {
            self.toast_overlay.add_toast(
                adw::Toast::builder()
                    .title("Pick a note first")
                    .timeout(2)
                    .build(),
            );
            return;
        };
        // Save any pending edits so the export reflects what's on screen.
        self.save_pending();
        let note = match self
            .state
            .borrow()
            .notes
            .iter()
            .find(|n| n.id == id)
            .cloned()
        {
            Some(n) => n,
            None => return,
        };

        let suggested = format!("{}.md", crate::export::slugify_title(&note.title, note.id));
        let docs =
            glib::user_special_dir(glib::UserDirectory::Documents).unwrap_or_else(glib::home_dir);
        let initial_folder = gio::File::for_path(&docs);

        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Markdown"));
        filter.add_suffix("md");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);

        let dialog = gtk::FileDialog::builder()
            .title("Export note")
            .accept_label("Export")
            .modal(true)
            .initial_folder(&initial_folder)
            .initial_name(&suggested)
            .filters(&filters)
            .build();

        let win = self.clone();
        let window = self.window.clone();
        glib::spawn_future_local(async move {
            match dialog.save_future(Some(&window)).await {
                Ok(file) => {
                    if let Some(path) = file.path() {
                        let toast = match crate::export::export_note_md(&note, &path, true) {
                            Ok(()) => adw::Toast::builder()
                                .title(format!("Exported to {}", path.display()))
                                .timeout(3)
                                .build(),
                            Err(e) => adw::Toast::builder()
                                .title(format!("Export failed: {e}"))
                                .timeout(5)
                                .build(),
                        };
                        win.toast_overlay.add_toast(toast);
                    }
                }
                Err(e) if e.matches(gtk::DialogError::Dismissed) => {}
                Err(e) => tracing::warn!("save dialog: {e}"),
            }
        });
    }

    fn export_all_notes(self: &Rc<Self>) {
        self.save_pending();
        let notes = self.state.borrow().notes.clone();
        if notes.is_empty() {
            self.toast_overlay.add_toast(
                adw::Toast::builder()
                    .title("No notes to export")
                    .timeout(2)
                    .build(),
            );
            return;
        }
        let docs =
            glib::user_special_dir(glib::UserDirectory::Documents).unwrap_or_else(glib::home_dir);
        let initial_folder = gio::File::for_path(&docs);

        let dialog = gtk::FileDialog::builder()
            .title("Export all notes — pick destination folder")
            .accept_label("Export here")
            .modal(true)
            .initial_folder(&initial_folder)
            .build();

        let win = self.clone();
        let window = self.window.clone();
        glib::spawn_future_local(async move {
            match dialog.select_folder_future(Some(&window)).await {
                Ok(file) => {
                    if let Some(folder) = file.path() {
                        let target = folder.join(format!(
                            "jot-export-{}",
                            chrono::Local::now().format("%Y-%m-%d")
                        ));
                        let toast = match crate::export::export_all_md(&notes, &target, false) {
                            Ok(n) => adw::Toast::builder()
                                .title(format!("Exported {n} note(s) to {}", target.display()))
                                .timeout(4)
                                .build(),
                            Err(e) => adw::Toast::builder()
                                .title(format!("Export failed: {e}"))
                                .timeout(5)
                                .build(),
                        };
                        win.toast_overlay.add_toast(toast);
                    }
                }
                Err(e) if e.matches(gtk::DialogError::Dismissed) => {}
                Err(e) => tracing::warn!("folder dialog: {e}"),
            }
        });
    }

    fn install_drop_target(self: &Rc<Self>) {
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let win = self.clone();
        drop.connect_drop(move |_, value, _, _| {
            let file_list = match value.get::<gdk::FileList>() {
                Ok(fl) => fl,
                Err(_) => return false,
            };
            let mut handled = false;
            for file in file_list.files() {
                if win.handle_dropped_file(&file) {
                    handled = true;
                }
            }
            handled
        });
        self.text_view.add_controller(drop);
    }

    /// Returns true if the file was consumed (i.e. inserted as image or note).
    fn handle_dropped_file(self: &Rc<Self>, file: &gio::File) -> bool {
        let path = match file.path() {
            Some(p) => p,
            None => return false,
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .unwrap_or_default();

        if DROP_IMAGE_EXT.contains(&ext.as_str()) {
            self.absorb_dropped_image(&path, &ext)
        } else if DROP_TEXT_EXT.contains(&ext.as_str()) {
            self.absorb_dropped_text(&path)
        } else {
            tracing::info!("ignoring drop with extension '{ext}': {}", path.display());
            false
        }
    }

    fn absorb_dropped_image(self: &Rc<Self>, source: &Path, ext: &str) -> bool {
        if self.state.borrow().current_id.is_none() {
            self.new_note();
        }
        let images = match images_dir() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("images dir: {e}");
                return false;
            }
        };
        let dest = images.join(format!("{}.{}", uuid::Uuid::new_v4(), ext));
        if let Err(e) = std::fs::copy(source, &dest) {
            tracing::warn!("copy dropped image: {e}");
            return false;
        }
        match gdk::Texture::from_filename(&dest) {
            Ok(texture) => {
                self.insert_image_at_cursor(&dest, &texture);
                true
            }
            Err(e) => {
                tracing::warn!("dropped image rejected by gdk: {e}");
                let _ = std::fs::remove_file(&dest);
                false
            }
        }
    }

    fn absorb_dropped_text(self: &Rc<Self>, source: &Path) -> bool {
        let content = match std::fs::read_to_string(source) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("read dropped text: {e}");
                return false;
            }
        };
        // Use filename stem as a fallback title if the file's first non-empty
        // line wouldn't make a useful one.
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Note");
        let body = if Note::derive_title(&content).is_empty() {
            format!("{stem}\n\n{content}")
        } else {
            content
        };
        let note = match self.db.create_note() {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("create dropped note: {e}");
                return false;
            }
        };
        let id = note.id;
        let title = Note::derive_title(&body);
        if let Err(e) = self.db.update_note(id, &title, &body) {
            tracing::error!("save dropped note: {e}");
            return false;
        }
        let mut updated = note;
        updated.title = title;
        updated.body = body;
        self.state.borrow_mut().notes.insert(0, updated);
        self.rebuild_list();
        self.select_note(id);
        true
    }
}

#[derive(Debug, PartialEq, Eq)]
enum BodyPart {
    Text(String),
    Image(String),
    CopyBlock(String),
}

fn normalize_note_tag(tag: &str) -> String {
    tag.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect()
}

fn normalize_note_color(color: &str) -> String {
    let trimmed = color.trim();
    if NOTE_COLORS.iter().any(|(id, _)| *id == trimmed) {
        trimmed.to_string()
    } else {
        String::new()
    }
}

fn note_color_css_class(color: &str) -> String {
    let color = normalize_note_color(color);
    if color.is_empty() {
        "jot-color-default".to_string()
    } else {
        format!("jot-color-{color}")
    }
}

fn collect_note_tags_with_counts(notes: &[Note]) -> Vec<(String, usize)> {
    let mut tags: Vec<(String, usize)> = Vec::new();
    for note in notes {
        let tag = normalize_note_tag(&note.tag);
        if tag.is_empty() {
            continue;
        }
        if let Some(existing) = tags
            .iter_mut()
            .find(|(t, _)| t.eq_ignore_ascii_case(&tag))
        {
            existing.1 += 1;
        } else {
            tags.push((tag, 1));
        }
    }
    tags.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    tags
}

fn collect_note_tags(notes: &[Note]) -> (Vec<String>, bool) {
    let mut tags: Vec<String> = Vec::new();
    let mut has_untagged = false;

    for note in notes {
        let tag = normalize_note_tag(&note.tag);
        if tag.is_empty() {
            has_untagged = true;
            continue;
        }
        if !tags
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&tag))
        {
            tags.push(tag);
        }
    }

    tags.sort_by_key(|tag| tag.to_lowercase());
    (tags, has_untagged)
}

fn note_matches_tag_filter(note: &Note, active_tag: Option<&str>) -> bool {
    match active_tag {
        None => true,
        Some("") => normalize_note_tag(&note.tag).is_empty(),
        Some(tag) => normalize_note_tag(&note.tag).eq_ignore_ascii_case(tag),
    }
}

/// Find every `http://...` / `https://...` run in `text`. Returns
/// (char_offset, char_length) tuples, where offsets count Unicode scalars
/// (matching `gtk::TextBuffer::iter_at_offset`).
fn find_urls(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let prefix_len = url_prefix_len(&chars[i..]);
        if prefix_len == 0 {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + prefix_len;
        while end < n && is_url_char(chars[end]) {
            end += 1;
        }
        while end > start + prefix_len && is_trailing_punct(chars[end - 1]) {
            end -= 1;
        }
        if end > start + prefix_len {
            out.push((start, end - start));
        }
        i = end.max(i + 1);
    }
    out
}

fn url_prefix_len(chars: &[char]) -> usize {
    if chars.starts_with(&['h', 't', 't', 'p', 's', ':', '/', '/']) {
        8
    } else if chars.starts_with(&['h', 't', 't', 'p', ':', '/', '/']) {
        7
    } else {
        0
    }
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, ')' | ']' | '<' | '>' | '"' | '\'' | '`')
}

fn is_trailing_punct(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?')
}

fn scale_to_fit(tex_w: i32, tex_h: i32, max_w: i32, max_h: i32) -> (i32, i32) {
    if tex_w <= 0 || tex_h <= 0 {
        return (max_w.min(160), max_h.min(160));
    }
    if tex_w <= max_w && tex_h <= max_h {
        return (tex_w, tex_h);
    }
    let scale_w = max_w as f64 / tex_w as f64;
    let scale_h = max_h as f64 / tex_h as f64;
    let scale = scale_w.min(scale_h);
    let w = ((tex_w as f64) * scale).round() as i32;
    let h = ((tex_h as f64) * scale).round() as i32;
    (w.max(40), h.max(40))
}

fn parse_markdown_body(body: &str) -> Vec<BodyPart> {
    const PREFIX: &[u8] = b"![image](";
    const COPY_OPEN: &[u8] = b"{copy}";
    const COPY_CLOSE: &str = "{/copy}";
    let bytes = body.as_bytes();
    let mut parts = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(PREFIX) {
            let after = i + PREFIX.len();
            if let Some(rel) = bytes[after..].iter().position(|&b| b == b')') {
                let end = after + rel;
                if !buf.is_empty() {
                    parts.push(BodyPart::Text(std::mem::take(&mut buf)));
                }
                parts.push(BodyPart::Image(body[after..end].to_string()));
                i = end + 1;
                continue;
            }
        }
        if bytes[i..].starts_with(COPY_OPEN) {
            let after = i + COPY_OPEN.len();
            if let Some(rel) = body[after..].find(COPY_CLOSE) {
                let end = after + rel;
                if !buf.is_empty() {
                    parts.push(BodyPart::Text(std::mem::take(&mut buf)));
                }
                parts.push(BodyPart::CopyBlock(body[after..end].to_string()));
                i = end + COPY_CLOSE.len();
                continue;
            }
        }
        let ch = body[i..].chars().next().unwrap();
        buf.push(ch);
        i += ch.len_utf8();
    }
    if !buf.is_empty() {
        parts.push(BodyPart::Text(buf));
    }
    parts
}

fn copy_block_payload(content: &str) -> String {
    let without_leading = content
        .strip_prefix("\r\n")
        .or_else(|| content.strip_prefix('\n'))
        .unwrap_or(content);
    without_leading
        .strip_suffix("\r\n")
        .or_else(|| without_leading.strip_suffix('\n'))
        .unwrap_or(without_leading)
        .to_string()
}

fn copy_blocks_before_char_offset(body: &str, char_offset: i32) -> i32 {
    const COPY_OPEN: &str = "{copy}";
    const COPY_CLOSE: &str = "{/copy}";

    let limit = body
        .char_indices()
        .nth(char_offset.max(0) as usize)
        .map(|(idx, _)| idx)
        .unwrap_or(body.len());
    let mut count = 0;
    let mut byte_pos = 0;

    while let Some(rel_open) = body[byte_pos..].find(COPY_OPEN) {
        let open = byte_pos + rel_open;
        let after_open = open + COPY_OPEN.len();
        let Some(rel_close) = body[after_open..].find(COPY_CLOSE) else {
            break;
        };
        if open < limit {
            count += 1;
        }
        byte_pos = after_open + rel_close + COPY_CLOSE.len();
    }

    count
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

impl JotWindow {
    fn build_note_row(self: &Rc<Self>, note: &Note) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();

        // Outer = [color strip | text content (expand) | pin star]
        let outer = gtk::Box::new(gtk::Orientation::Horizontal, 6);

        let color_strip = gtk::Box::new(gtk::Orientation::Vertical, 0);
        color_strip.add_css_class("jot-row-color");
        color_strip.add_css_class(&note_color_css_class(&note.color));
        color_strip.set_vexpand(true);
        color_strip.set_valign(gtk::Align::Fill);
        outer.append(&color_strip);

        let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bx.set_hexpand(true);

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
            .label(format!("{}", local.format("%d %b · %H:%M")))
            .halign(gtk::Align::Start)
            .build();
        time.add_css_class("jot-row-time");

        let tag_text = normalize_note_tag(&note.tag);
        bx.append(&title);
        if !snippet_text.is_empty() {
            bx.append(&snippet);
        }
        if !tag_text.is_empty() {
            let tag = gtk::Label::builder()
                .label(format!("#{tag_text}"))
                .halign(gtk::Align::Start)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .lines(1)
                .build();
            tag.add_css_class("jot-row-tag");
            bx.append(&tag);
        }
        bx.append(&time);

        // Pin star — solid when pinned, hollow when not. Click toggles.
        let icon_name = if note.pinned {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        };
        let pin_btn = gtk::Button::builder()
            .icon_name(icon_name)
            .has_frame(false)
            .tooltip_text(if note.pinned { "Unpin" } else { "Pin to top" })
            .valign(gtk::Align::Center)
            .build();
        pin_btn.add_css_class("jot-pin-btn");
        if note.pinned {
            pin_btn.add_css_class("jot-pin-active");
        }
        let win = self.clone();
        let note_id = note.id;
        let was_pinned = note.pinned;
        pin_btn.connect_clicked(move |_| {
            win.toggle_pin(note_id, !was_pinned);
        });

        outer.append(&bx);
        outer.append(&pin_btn);
        row.set_child(Some(&outer));

        unsafe {
            row.set_data("note-id", note.id);
        }
        row
    }

    fn toggle_pin(self: &Rc<Self>, id: i64, pinned: bool) {
        if let Err(e) = self.db.set_pinned(id, pinned) {
            tracing::error!("set_pinned failed: {e}");
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            if let Some(note) = state.notes.iter_mut().find(|n| n.id == id) {
                note.pinned = pinned;
            }
            state.notes.sort_by(|a, b| {
                b.pinned
                    .cmp(&a.pinned)
                    .then(b.updated_at.cmp(&a.updated_at))
            });
        }
        self.rebuild_list();
    }

    fn set_current_tag(self: &Rc<Self>, tag: String) {
        let Some(id) = self.state.borrow().current_id else {
            return;
        };

        let already_current = self
            .state
            .borrow()
            .notes
            .iter()
            .find(|n| n.id == id)
            .is_some_and(|n| n.tag == tag);
        if already_current {
            return;
        }

        if let Err(e) = self.db.set_tag(id, &tag) {
            tracing::error!("set_tag failed: {e}");
            self.toast_overlay.add_toast(
                adw::Toast::builder()
                    .title(format!("Tag update failed: {e}"))
                    .timeout(4)
                    .build(),
            );
            return;
        }

        if let Some(note) = self
            .state
            .borrow_mut()
            .notes
            .iter_mut()
            .find(|n| n.id == id)
        {
            note.tag = tag.clone();
        }
        self.update_tag_picker_label(&tag);
        self.rebuild_tag_tabs();
        self.rebuild_list();
    }

    fn update_tag_picker_label(&self, tag: &str) {
        if tag.is_empty() {
            self.tag_picker_label.set_text("Add tag…");
            self.tag_picker_label.add_css_class("jot-tag-picker-label-empty");
        } else {
            self.tag_picker_label.set_text(tag);
            self.tag_picker_label.remove_css_class("jot-tag-picker-label-empty");
        }
    }

    fn current_note_tag(&self) -> String {
        let state = self.state.borrow();
        state
            .current_id
            .and_then(|id| state.notes.iter().find(|n| n.id == id))
            .map(|n| normalize_note_tag(&n.tag))
            .unwrap_or_default()
    }

    fn refresh_tag_picker_list(self: &Rc<Self>) {
        while let Some(child) = self.tag_picker_list.first_child() {
            self.tag_picker_list.remove(&child);
        }
        self.tag_picker_actions.borrow_mut().clear();

        let query = self.tag_picker_search.text().to_string();
        let query_trim = query.trim();
        let query_lower = query_trim.to_lowercase();

        let all = collect_note_tags_with_counts(&self.state.borrow().notes);
        let current = self.current_note_tag();
        let current_lower = current.to_lowercase();

        if query_lower.is_empty() || "no tag".contains(&query_lower) {
            let row = self.build_tag_picker_row(None, 0, current.is_empty());
            self.tag_picker_list.append(&row);
            self.tag_picker_actions
                .borrow_mut()
                .push(TagPickerAction::Apply(String::new()));
        }

        let mut has_exact = false;
        for (tag, count) in &all {
            if !query_lower.is_empty() && !tag.to_lowercase().contains(&query_lower) {
                continue;
            }
            if tag.to_lowercase() == query_lower {
                has_exact = true;
            }
            let is_current = tag.to_lowercase() == current_lower;
            let row = self.build_tag_picker_row(Some(tag.clone()), *count, is_current);
            self.tag_picker_list.append(&row);
            self.tag_picker_actions
                .borrow_mut()
                .push(TagPickerAction::Apply(tag.clone()));
        }

        if !query_trim.is_empty() && !has_exact {
            let normalized = normalize_note_tag(query_trim);
            if !normalized.is_empty() {
                let row = self.build_tag_picker_create_row(&normalized);
                self.tag_picker_list.append(&row);
                self.tag_picker_actions
                    .borrow_mut()
                    .push(TagPickerAction::Create(normalized));
            }
        }

        if let Some(first) = self.tag_picker_list.row_at_index(0) {
            self.tag_picker_list.select_row(Some(&first));
        }
    }

    fn build_tag_picker_row(
        self: &Rc<Self>,
        tag: Option<String>,
        count: usize,
        is_current: bool,
    ) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("jot-tag-picker-row");
        if is_current {
            row.add_css_class("jot-tag-picker-row-active");
        }
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hbox.set_margin_start(8);
        hbox.set_margin_end(8);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);

        let icon_name = if tag.is_none() {
            "edit-clear-symbolic"
        } else {
            "tag-outline-symbolic"
        };
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.add_css_class("jot-tag-picker-row-icon");
        hbox.append(&icon);

        let label = gtk::Label::builder()
            .label(tag.clone().unwrap_or_else(|| "No tag".to_string()))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        if tag.is_none() {
            label.add_css_class("jot-tag-picker-row-clear");
        }
        hbox.append(&label);

        if tag.is_some() && count > 0 {
            let count_label = gtk::Label::new(Some(&count.to_string()));
            count_label.add_css_class("jot-tag-picker-count");
            hbox.append(&count_label);
        }

        if is_current {
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.add_css_class("jot-tag-picker-check");
            hbox.append(&check);
        }

        row.set_child(Some(&hbox));
        row
    }

    fn build_tag_picker_create_row(self: &Rc<Self>, tag: &str) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("jot-tag-picker-row");
        row.add_css_class("jot-tag-picker-row-create");
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hbox.set_margin_start(8);
        hbox.set_margin_end(8);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);

        let icon = gtk::Image::from_icon_name("list-add-symbolic");
        icon.add_css_class("jot-tag-picker-row-icon");
        hbox.append(&icon);

        let label = gtk::Label::builder()
            .label(format!("Create \"{tag}\""))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        hbox.append(&label);

        row.set_child(Some(&hbox));
        row
    }

    fn move_tag_picker_selection(self: &Rc<Self>, delta: i32) {
        let current_idx = self
            .tag_picker_list
            .selected_row()
            .map(|r| r.index())
            .unwrap_or(-1);
        let mut target = current_idx + delta;
        if target < 0 {
            target = 0;
        }
        if let Some(row) = self.tag_picker_list.row_at_index(target) {
            self.tag_picker_list.select_row(Some(&row));
        }
    }

    fn activate_selected_tag_picker_row(self: &Rc<Self>) {
        let idx = match self.tag_picker_list.selected_row() {
            Some(row) => row.index() as usize,
            None => return,
        };
        let action = self.tag_picker_actions.borrow().get(idx).cloned();
        if let Some(action) = action {
            self.apply_tag_picker_action(action);
        }
    }

    fn apply_tag_picker_action(self: &Rc<Self>, action: TagPickerAction) {
        let target = match action {
            TagPickerAction::Apply(tag) => tag,
            TagPickerAction::Create(tag) => tag,
        };
        self.tag_picker_popover.popdown();
        self.set_current_tag(normalize_note_tag(&target));
    }

    fn set_current_color(self: &Rc<Self>, color: &str) {
        let Some(id) = self.state.borrow().current_id else {
            return;
        };
        let color = normalize_note_color(color);

        let already_current = self
            .state
            .borrow()
            .notes
            .iter()
            .find(|n| n.id == id)
            .is_some_and(|n| normalize_note_color(&n.color) == color);
        if already_current {
            self.update_note_metadata_controls();
            return;
        }

        if let Err(e) = self.db.set_color(id, &color) {
            tracing::error!("set_color failed: {e}");
            self.toast_overlay.add_toast(
                adw::Toast::builder()
                    .title(format!("Color update failed: {e}"))
                    .timeout(4)
                    .build(),
            );
            return;
        }

        if let Some(note) = self
            .state
            .borrow_mut()
            .notes
            .iter_mut()
            .find(|n| n.id == id)
        {
            note.color = color;
        }
        self.update_note_metadata_controls();
        self.rebuild_list();
    }

    fn update_note_metadata_controls(&self) {
        let current = self.state.borrow().current_id;
        let note = current.and_then(|id| {
            self.state
                .borrow()
                .notes
                .iter()
                .find(|n| n.id == id)
                .map(|n| (n.tag.clone(), normalize_note_color(&n.color)))
        });

        self.suppress.set(true);
        match note {
            Some((tag, color)) => {
                self.tag_picker_button.set_sensitive(true);
                self.update_tag_picker_label(&tag);
                for (button_color, button) in self.color_buttons.borrow().iter() {
                    button.set_sensitive(true);
                    button.set_active(button_color == &color);
                }
            }
            None => {
                self.tag_picker_button.set_sensitive(false);
                self.update_tag_picker_label("");
                for (_, button) in self.color_buttons.borrow().iter() {
                    button.set_sensitive(false);
                    button.set_active(false);
                }
            }
        }
        self.suppress.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        collect_note_tags, copy_block_payload, copy_blocks_before_char_offset,
        normalize_note_color, normalize_note_tag, note_matches_tag_filter, parse_markdown_body,
        BodyPart,
    };
    use crate::note::Note;
    use chrono::Utc;

    fn note(tag: &str) -> Note {
        Note {
            id: 1,
            title: String::new(),
            body: String::new(),
            tag: tag.to_string(),
            color: String::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            pinned: false,
        }
    }

    #[test]
    fn parses_copy_blocks_alongside_text_and_images() {
        assert_eq!(
            parse_markdown_body("before {copy}hello{/copy}\n![image](/tmp/a.png)"),
            vec![
                BodyPart::Text("before ".to_string()),
                BodyPart::CopyBlock("hello".to_string()),
                BodyPart::Text("\n".to_string()),
                BodyPart::Image("/tmp/a.png".to_string()),
            ]
        );
    }

    #[test]
    fn leaves_unclosed_copy_blocks_as_text() {
        assert_eq!(
            parse_markdown_body("{copy}hello"),
            vec![BodyPart::Text("{copy}hello".to_string())]
        );
    }

    #[test]
    fn normalizes_multiline_copy_payload_edges() {
        assert_eq!(copy_block_payload("\nhello\n"), "hello");
        assert_eq!(copy_block_payload("\r\nhello\r\n"), "hello");
        assert_eq!(copy_block_payload("hello\nworld"), "hello\nworld");
    }

    #[test]
    fn counts_copy_anchors_inserted_before_cursor() {
        let body = "a {copy}one{/copy} b {copy}two{/copy}";
        assert_eq!(copy_blocks_before_char_offset(body, 1), 0);
        assert_eq!(copy_blocks_before_char_offset(body, 20), 1);
        assert_eq!(
            copy_blocks_before_char_offset(body, body.chars().count() as i32),
            2
        );
    }

    #[test]
    fn normalizes_tags_and_colors_for_filters() {
        assert_eq!(normalize_note_tag("  Work\n"), "Work");
        assert_eq!(normalize_note_color("purple"), "purple");
        assert_eq!(normalize_note_color("chartreuse"), "");

        let notes = vec![note("Work"), note("work"), note(""), note("Personal")];
        let (tags, has_untagged) = collect_note_tags(&notes);
        assert_eq!(tags, vec!["Personal".to_string(), "Work".to_string()]);
        assert!(has_untagged);
        assert!(note_matches_tag_filter(&notes[0], Some("work")));
        assert!(note_matches_tag_filter(&notes[2], Some("")));
        assert!(note_matches_tag_filter(&notes[3], None));
    }
}
