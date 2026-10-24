//! The History window: saved sessions, a transcript reader with search,
//! copy and export.
//!
//! Every file operation runs off the main thread. Each kind of request has
//! a generation counter, and a result that a newer request superseded is
//! dropped.

use std::{
    cell::{Cell, RefCell},
    ops::Range,
    path::PathBuf,
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gtk::{gdk, gio, glib, pango, prelude::*};
use lcrt_core::history::{
    self, ExportContent, ExportFormat, HistorySession, HistoryStore, Library, SessionHeader,
    SessionId, SessionStatus, SessionSummary,
};
use libadwaita::{self as adw, prelude::*};

use crate::HistoryStatus;

/// Transcript pieces inserted per idle iteration while a reader fills.
const FILL_BATCH: usize = 600;
const LATE_NOTE: &str = "Saving began after captions had started; earlier speech isn't included.";
const GAP_NOTE: &str = "Some speech here was not saved.";
const NO_TRANSLATION_NOTE: &str = "Translation not saved";
const INTERRUPTED_NOTE: &str =
    "This session was interrupted (LILOPOP quit or crashed) — what was saved before that is shown.";

/// The Caption History window.
pub(crate) struct HistoryWindow {
    me: Weak<Self>,
    window: adw::Window,
    toasts: adw::ToastOverlay,
    navigation: adw::NavigationView,
    library_page: adw::NavigationPage,
    store: HistoryStore,
    open_settings: Box<dyn Fn()>,
    status: RefCell<HistoryStatus>,
    /// The last library read, or why it couldn't be read.
    library: RefCell<Result<Library, String>>,
    /// Sessions matching the search; `None` shows every session.
    found: RefCell<Option<Vec<SessionId>>>,
    list_generation: Cell<u64>,
    search_generation: Cell<u64>,
    search_cancel: RefCell<Arc<AtomicBool>>,
    load_generation: Cell<u64>,
    reader: RefCell<Option<Rc<Reader>>>,
    search: gtk::SearchEntry,
    saving_note: gtk::Label,
    off_note: gtk::Label,
    unreadable_note: gtk::Label,
    stack: gtk::Stack,
    groups: gtk::Box,
    footer: gtk::Label,
    empty: adw::StatusPage,
    empty_settings: gtk::Button,
}

/// What to do with a session once it is loaded.
#[derive(Clone, Copy)]
enum Then {
    Read,
    Export,
}

impl HistoryWindow {
    /// Creates the window, transient for `parent`. `open_settings` opens
    /// Settings → History & Privacy.
    pub(crate) fn new(
        parent: &adw::ApplicationWindow,
        store: HistoryStore,
        open_settings: impl Fn() + 'static,
    ) -> Rc<Self> {
        let this = Rc::new_cyclic(|me: &Weak<Self>| {
            let window = adw::Window::builder()
                .transient_for(parent)
                .title("Caption History")
                .hide_on_close(true)
                .default_width(560)
                .default_height(640)
                .build();
            let settings = icon_button("emblem-system-symbolic", "History settings");
            let header = adw::HeaderBar::new();
            header.pack_end(&settings);
            let me_settings = me.clone();
            settings.connect_clicked(move |_| {
                if let Some(this) = me_settings.upgrade() {
                    (this.open_settings)();
                }
            });

            let search = gtk::SearchEntry::builder()
                .placeholder_text("Search saved transcripts…")
                .build();
            let saving_note = note_label(&["warning"]);
            let off_note = note_label(&["dim-label"]);
            off_note.set_label("Saving is off. These sessions were saved earlier.");
            let unreadable_note = note_label(&["dim-label"]);
            let top = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .margin_top(12)
                .margin_start(12)
                .margin_end(12)
                .build();
            top.append(&search);
            top.append(&saving_note);
            top.append(&off_note);
            top.append(&unreadable_note);

            let groups = gtk::Box::new(gtk::Orientation::Vertical, 18);
            let footer = gtk::Label::builder()
                .css_classes(["dim-label", "caption"])
                .build();
            let list = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(18)
                .margin_top(12)
                .margin_bottom(18)
                .margin_start(12)
                .margin_end(12)
                .build();
            list.append(&groups);
            list.append(&footer);
            let scrolled = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&clamped(&list))
                .build();

            let empty_settings = gtk::Button::builder()
                .label("Open Settings")
                .halign(gtk::Align::Center)
                .css_classes(["pill", "suggested-action"])
                .build();
            let me_empty = me.clone();
            empty_settings.connect_clicked(move |_| {
                if let Some(this) = me_empty.upgrade() {
                    (this.open_settings)();
                }
            });
            let empty = adw::StatusPage::builder()
                .icon_name("document-open-recent-symbolic")
                .child(&empty_settings)
                .vexpand(true)
                .build();
            let stack = gtk::Stack::new();
            stack.add_named(&scrolled, Some("list"));
            stack.add_named(&empty, Some("empty"));

            let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
            body.append(&clamped(&top));
            body.append(&stack);
            let toolbar = adw::ToolbarView::new();
            toolbar.add_top_bar(&header);
            toolbar.set_content(Some(&body));
            let library_page = adw::NavigationPage::builder()
                .title("Caption History")
                .tag("library")
                .child(&toolbar)
                .build();
            let navigation = adw::NavigationView::new();
            navigation.add(&library_page);
            let toasts = adw::ToastOverlay::new();
            toasts.set_child(Some(&navigation));
            window.set_content(Some(&toasts));

            Self {
                me: me.clone(),
                window,
                toasts,
                navigation,
                library_page,
                store,
                open_settings: Box::new(open_settings),
                status: RefCell::new(HistoryStatus::Off),
                library: RefCell::new(Ok(Library::default())),
                found: RefCell::new(None),
                list_generation: Cell::new(0),
                search_generation: Cell::new(0),
                search_cancel: RefCell::new(Arc::new(AtomicBool::new(false))),
                load_generation: Cell::new(0),
                reader: RefCell::new(None),
                search,
                saving_note,
                off_note,
                unreadable_note,
                stack,
                groups,
                footer,
                empty,
                empty_settings,
            }
        });

        let weak = Rc::downgrade(&this);
        this.search.connect_search_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.start_search();
            }
        });
        this.search.connect_stop_search(|entry| entry.set_text(""));
        let weak = Rc::downgrade(&this);
        this.navigation.connect_popped(move |_, page| {
            let Some(this) = weak.upgrade() else { return };
            let shown = this
                .reader
                .borrow()
                .as_ref()
                .is_some_and(|reader| &reader.page == page);
            if shown {
                this.reader.replace(None);
            }
        });
        this.render();
        this
    }

    /// Shows the window, reloading the library.
    pub(crate) fn present(&self) {
        self.reload();
        self.window.present();
    }

    /// What history is doing now, including which session is recording.
    pub(crate) fn set_status(&self, status: &HistoryStatus) {
        let previous = self.status.replace(status.clone());
        if recording_id(&previous) != recording_id(status) && self.window.is_visible() {
            self.reload();
        } else {
            self.render();
        }
    }

    fn toast(&self, title: &str) {
        let toast = adw::Toast::new(title);
        toast.set_use_markup(false);
        self.toasts.add_toast(toast);
    }

    /// Lists the saved sessions in the background, then searches or shows
    /// them.
    fn reload(&self) {
        let generation = bump(&self.list_generation);
        let store = self.store.clone();
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || store.list()).await;
            let Some(this) = me.upgrade() else { return };
            if this.list_generation.get() != generation {
                return;
            }
            *this.library.borrow_mut() = match result {
                Ok(Ok(library)) => Ok(library),
                Ok(Err(error)) => Err(error.kind().to_string()),
                Err(_) => Err("reading stopped unexpectedly".to_owned()),
            };
            this.start_search();
        });
    }

    /// Searches the last library for the query, cancelling any earlier
    /// search. An empty query shows every session.
    fn start_search(&self) {
        self.search_cancel.borrow().store(true, Ordering::Relaxed);
        let generation = bump(&self.search_generation);
        let query = self.search.text().trim().to_owned();
        let library = match &*self.library.borrow() {
            Ok(library) if !query.is_empty() => Some(library.clone()),
            _ => None,
        };
        let Some(library) = library else {
            self.found.replace(None);
            self.render();
            return;
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.search_cancel.replace(cancelled.clone());
        let store = self.store.clone();
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            let found =
                gio::spawn_blocking(move || store.search(&library, &query, &cancelled)).await;
            let Some(this) = me.upgrade() else { return };
            if this.search_generation.get() != generation {
                return;
            }
            this.found.replace(Some(found.unwrap_or_default()));
            this.render();
        });
    }

    /// Rebuilds the library page from the last library, search and status.
    fn render(&self) {
        let status = self.status.borrow().clone();
        let saving = match &status {
            HistoryStatus::StorageFull => Some("history reached its storage limit.".to_owned()),
            HistoryStatus::Failed(message) => Some(message.clone()),
            _ => None,
        };
        self.saving_note.set_label(&format!(
            "Not saving: {}",
            saving.as_deref().unwrap_or_default()
        ));
        self.saving_note.set_visible(saving.is_some());
        while let Some(child) = self.groups.first_child() {
            self.groups.remove(&child);
        }
        let library = self.library.borrow();
        let library = match &*library {
            Ok(library) => library,
            Err(kind) => {
                self.off_note.set_visible(false);
                self.unreadable_note.set_visible(false);
                self.show_empty("History can't be read", &format!("Error: {kind}."), false);
                return;
            }
        };
        self.off_note
            .set_visible(status == HistoryStatus::Off && !library.sessions.is_empty());
        self.unreadable_note
            .set_label(&unreadable_label(library.unreadable));
        self.unreadable_note.set_visible(library.unreadable > 0);

        let found = self.found.borrow();
        let visible: Vec<&SessionSummary> = library
            .sessions
            .iter()
            .filter(|summary| found.as_ref().is_none_or(|ids| ids.contains(&summary.id)))
            .collect();
        if visible.is_empty() {
            if found.is_some() {
                let query = self.search.text();
                let description = format!(
                    "No saved transcript contains “{}”.",
                    glib::markup_escape_text(query.trim())
                );
                self.show_empty("No matches", &description, false);
            } else if status == HistoryStatus::Off {
                self.show_empty(
                    "Caption history is off",
                    "Turn on “Save caption history” in Settings to keep the text of future \
                     sessions on this device. Earlier sessions weren't saved and can't be \
                     recovered.",
                    true,
                );
            } else {
                self.show_empty(
                    "No saved sessions yet",
                    "Start captions and they will appear here.",
                    false,
                );
            }
        } else {
            self.stack.set_visible_child_name("list");
            let now = glib::DateTime::now_local().ok();
            let today = now.as_ref().and_then(|now| now.format("%Y-%m-%d").ok());
            let yesterday = now
                .and_then(|now| now.add_days(-1).ok())
                .and_then(|day| day.format("%Y-%m-%d").ok());
            let mut day = None;
            let mut group = adw::PreferencesGroup::new();
            for summary in visible {
                let started = summary.header.started_at_ms;
                let key = local_format(started, "%Y-%m-%d");
                if day.as_ref() != Some(&key) {
                    let heading = day_heading(
                        &key,
                        today.as_deref().unwrap_or_default(),
                        yesterday.as_deref().unwrap_or_default(),
                        &local_format(started, "%a %-d %b %Y"),
                    );
                    group = adw::PreferencesGroup::builder().title(heading).build();
                    self.groups.append(&group);
                    day = Some(key);
                }
                group.add(&self.session_row(summary, recording_id(&status)));
            }
        }
        let bytes = library
            .sessions
            .iter()
            .map(|summary| summary.size_bytes)
            .sum();
        self.footer
            .set_label(&library_footer(library.sessions.len(), bytes));

        // The session being read was deleted.
        let shown = self
            .reader
            .borrow()
            .as_ref()
            .map(|reader| reader.session.id.clone());
        if shown.is_some_and(|id| !library.sessions.iter().any(|summary| summary.id == id)) {
            self.navigation.pop_to_page(&self.library_page);
        }
    }

    fn show_empty(&self, title: &str, description: &str, settings: bool) {
        self.empty.set_title(title);
        self.empty.set_description(Some(description));
        self.empty_settings.set_visible(settings);
        self.stack.set_visible_child_name("empty");
    }

    /// One session in the library. It never shows transcript text.
    fn session_row(
        &self,
        summary: &SessionSummary,
        recording: Option<&SessionId>,
    ) -> adw::ActionRow {
        let active = recording == Some(&summary.id);
        let status = SessionStatus::of(&summary.header, summary.end.as_ref(), summary.gaps, active);
        let time = local_format(summary.header.started_at_ms, "%H:%M");
        let row = adw::ActionRow::builder()
            .title(session_title(&summary.header))
            .subtitle(session_subtitle(&time, summary, status))
            .use_markup(false)
            .activatable(true)
            .build();
        let export = icon_button("document-save-symbolic", "Export");
        let delete = icon_button("user-trash-symbolic", "Delete");
        if active {
            delete.set_sensitive(false);
            delete.set_tooltip_text(Some("Being saved now"));
            delete.update_property(&[gtk::accessible::Property::Label(
                "Delete (unavailable: being saved now)",
            )]);
        }
        for button in [&export, &delete] {
            button.set_valign(gtk::Align::Center);
            row.add_suffix(button);
        }
        let open = |then: Then| {
            let (me, id) = (self.me.clone(), summary.id.clone());
            move || {
                if let Some(this) = me.upgrade() {
                    this.open_session(id.clone(), then);
                }
            }
        };
        let read = open(Then::Read);
        row.connect_activated(move |_| read());
        let export_session = open(Then::Export);
        export.connect_clicked(move |_| export_session());
        let (me, id) = (self.me.clone(), summary.id.clone());
        delete.connect_clicked(move |_| {
            if let Some(this) = me.upgrade() {
                this.confirm_delete(id.clone());
            }
        });
        row
    }

    /// Loads a session in the background, then reads or exports it.
    fn open_session(&self, id: SessionId, then: Then) {
        let generation = bump(&self.load_generation);
        let store = self.store.clone();
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || store.load(&id)).await;
            let Some(this) = me.upgrade() else { return };
            if this.load_generation.get() != generation {
                return;
            }
            match (result, then) {
                (Ok(Ok(session)), Then::Read) => this.show_reader(session),
                (Ok(Ok(session)), Then::Export) => this.open_export(Arc::new(session)),
                _ => this.toast("This session can't be read"),
            }
        });
    }

    fn show_reader(&self, session: HistorySession) {
        let active = recording_id(&self.status.borrow()) == Some(&session.id);
        let reader = Reader::new(self.me.clone(), session, active);
        self.navigation.pop_to_page(&self.library_page);
        self.reader.replace(Some(reader.clone()));
        self.navigation.push(&reader.page);
        reader.render();
    }

    fn confirm_delete(&self, id: SessionId) {
        let dialog = adw::MessageDialog::new(
            Some(&self.window),
            Some("Delete this session?"),
            Some("Its saved transcript is removed permanently."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let me = self.me.clone();
        dialog.connect_response(Some("delete"), move |_, _| {
            if let Some(this) = me.upgrade() {
                this.delete(id.clone());
            }
        });
        dialog.present();
    }

    fn delete(&self, id: SessionId) {
        // Recording may have started while the dialog was open.
        if recording_id(&self.status.borrow()) == Some(&id) {
            self.toast("This session is being saved now");
            return;
        }
        let store = self.store.clone();
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || store.delete(&id)).await;
            let Some(this) = me.upgrade() else { return };
            if !matches!(result, Ok(Ok(()))) {
                this.toast("Couldn't delete this session");
            }
            this.reload();
        });
    }

    /// The modal Export Transcript dialog for `session`.
    fn open_export(&self, session: Arc<HistorySession>) {
        let dialog = adw::Window::builder()
            .title("Export Transcript")
            .transient_for(&self.window)
            .modal(true)
            .destroy_with_parent(true)
            .default_width(420)
            .build();
        let formats: Vec<_> = ExportFormat::ALL
            .iter()
            .map(|format| (*format, format.label().to_owned()))
            .collect();
        let (format_group, format) = choice_group("Format", &formats);
        let contents: Vec<_> = session
            .export_contents()
            .into_iter()
            .map(|content| (content, content.label()))
            .collect();
        let (content_group, content) = choice_group("Content", &contents);
        let cancel = gtk::Button::with_label("Cancel");
        let export = gtk::Button::builder()
            .label("Export")
            .css_classes(["suggested-action"])
            .build();
        let buttons = gtk::Box::builder()
            .spacing(12)
            .halign(gtk::Align::End)
            .build();
        buttons.append(&cancel);
        buttons.append(&export);
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        body.append(&format_group);
        body.append(&content_group);
        body.append(&buttons);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&body));
        dialog.set_content(Some(&toolbar));

        let weak_dialog = dialog.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(dialog) = weak_dialog.upgrade() {
                dialog.close();
            }
        });
        let weak_dialog = dialog.downgrade();
        let me = self.me.clone();
        export.connect_clicked(move |_| {
            let Some(dialog) = weak_dialog.upgrade() else {
                return;
            };
            let (Some(format), Some(content)) = (format.get(), content.get()) else {
                return;
            };
            let started = session.header.started_at_ms;
            let name = format!(
                "lilopop-{}.{}",
                local_format(started, "%Y-%m-%d-%H%M"),
                format.extension()
            );
            let file_dialog = gtk::FileDialog::builder()
                .title("Export Transcript")
                .modal(true)
                .initial_name(name)
                .build();
            let (me, session, weak_dialog) = (me.clone(), session.clone(), dialog.downgrade());
            file_dialog.save(Some(&dialog), None::<&gio::Cancellable>, move |result| {
                let Some(this) = me.upgrade() else { return };
                match result {
                    Ok(file) => {
                        if let Some(dialog) = weak_dialog.upgrade() {
                            dialog.close();
                        }
                        match file.path() {
                            Some(path) => this.write_export(session, format, content, path),
                            None => this.toast("Couldn't export: choose a local folder."),
                        }
                    }
                    Err(error)
                        if error.matches(gtk::DialogError::Dismissed)
                            || error.matches(gtk::DialogError::Cancelled) => {}
                    Err(error) => this.toast(&format!("Couldn't export: {error}")),
                }
            });
        });
        dialog.present();
    }

    /// Builds the export and writes it to `path` in the background.
    fn write_export(
        &self,
        session: Arc<HistorySession>,
        format: ExportFormat,
        content: ExportContent,
        path: PathBuf,
    ) {
        let title = format!(
            "LILOPOP · {} · {}",
            reader_title(&session.header),
            local_format(session.header.started_at_ms, "%Y-%m-%d %H:%M")
        );
        let me = self.me.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                std::fs::write(path, history::export(&session, format, content, &title))
            })
            .await;
            let Some(this) = me.upgrade() else { return };
            match result {
                Ok(Ok(())) => this.toast("Exported"),
                Ok(Err(error)) => this.toast(&format!("Couldn't export: {error}")),
                Err(_) => this.toast("Couldn't export: writing stopped unexpectedly"),
            }
        });
    }
}

/// How a piece of the rendered transcript looks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Piece {
    /// Saved text.
    Text,
    /// A segment's elapsed time.
    Time,
    /// An explanation that is not part of the transcript.
    Note,
    /// A translation under its original.
    Translation,
    /// A missing translation under its original.
    MissingTranslation,
}

/// Text styles of the reader's buffer.
struct Tags {
    time: gtk::TextTag,
    note: gtk::TextTag,
    indent: gtk::TextTag,
    found: gtk::TextTag,
    current: gtk::TextTag,
}

/// The transcript reader page for one session.
struct Reader {
    history: Weak<HistoryWindow>,
    session: Arc<HistorySession>,
    languages: Vec<lcrt_core::Language>,
    interrupted: bool,
    page: adw::NavigationPage,
    translation: gtk::ToggleButton,
    bilingual: gtk::ToggleButton,
    language: gtk::DropDown,
    search: gtk::SearchEntry,
    count: gtk::Label,
    text_view: gtk::TextView,
    buffer: gtk::TextBuffer,
    mark: gtk::TextMark,
    tags: Tags,
    view: Cell<ExportContent>,
    /// The rendered transcript, also what Copy all copies.
    pieces: RefCell<Rc<Vec<(Piece, String)>>>,
    fill_generation: Cell<u64>,
    filled: Cell<bool>,
    matches: RefCell<Vec<Range<usize>>>,
    current: Cell<usize>,
}

impl Reader {
    fn new(history: Weak<HistoryWindow>, session: HistorySession, active: bool) -> Rc<Self> {
        let status = SessionStatus::of(&session.header, session.end.as_ref(), session.gaps, active);
        let languages = session.translation_languages();
        let copy = icon_button("edit-copy-symbolic", "Copy all");
        let export = icon_button("document-save-symbolic", "Export…");
        let header = adw::HeaderBar::new();
        header.pack_end(&export);
        header.pack_end(&copy);

        let original = gtk::ToggleButton::with_label("Original");
        original.set_active(true);
        let translation = gtk::ToggleButton::with_label("Translation");
        let bilingual = gtk::ToggleButton::with_label("Bilingual");
        let toggles = gtk::Box::builder().css_classes(["linked"]).build();
        translation.set_group(Some(&original));
        bilingual.set_group(Some(&original));
        for toggle in [&original, &translation, &bilingual] {
            toggles.append(toggle);
        }
        let names: Vec<&str> = languages.iter().map(|language| language.label()).collect();
        let language = gtk::DropDown::from_strings(&names);
        language.set_tooltip_text(Some("Translation language"));
        language.set_visible(languages.len() > 1);
        let selectors = gtk::Box::builder().spacing(6).build();
        selectors.append(&toggles);
        selectors.append(&language);
        selectors.set_visible(!languages.is_empty());
        let approximate = note_label(&["dim-label", "caption"]);
        approximate.set_label("Times are approximate.");
        approximate.set_visible(true);
        let recording = note_label(&["dim-label"]);
        recording.set_label("Still being saved — reopen to see newer text.");
        recording.set_visible(status == SessionStatus::Recording);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search this transcript…")
            .hexpand(true)
            .build();
        let previous = icon_button("go-up-symbolic", "Previous match");
        let next = icon_button("go-down-symbolic", "Next match");
        let steps = gtk::Box::builder().css_classes(["linked"]).build();
        for button in [&previous, &next] {
            button.remove_css_class("flat");
            steps.append(button);
        }
        let count = gtk::Label::builder()
            .css_classes(["dim-label"])
            .width_chars(9)
            .build();
        let search_row = gtk::Box::builder().spacing(6).build();
        search_row.append(&search);
        search_row.append(&steps);
        search_row.append(&count);
        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        for widget in [
            selectors.upcast_ref::<gtk::Widget>(),
            approximate.upcast_ref(),
            recording.upcast_ref(),
            search_row.upcast_ref(),
        ] {
            controls.append(widget);
        }

        let buffer = gtk::TextBuffer::new(None);
        let tags = Tags {
            time: gtk::TextTag::new(None),
            note: gtk::TextTag::builder().style(pango::Style::Italic).build(),
            indent: gtk::TextTag::builder().left_margin(66).build(),
            found: gtk::TextTag::builder()
                .background_rgba(&gdk::RGBA::new(1.0, 0.85, 0.0, 0.35))
                .build(),
            current: gtk::TextTag::builder()
                .background_rgba(&gdk::RGBA::new(1.0, 0.55, 0.0, 0.7))
                .build(),
        };
        for tag in [
            &tags.time,
            &tags.note,
            &tags.indent,
            &tags.found,
            &tags.current,
        ] {
            buffer.tag_table().add(tag);
        }
        let mark = buffer.create_mark(None, &buffer.start_iter(), true);
        let text_view = gtk::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(18)
            .right_margin(18)
            .top_margin(12)
            .bottom_margin(18)
            .pixels_below_lines(6)
            .build();
        text_view.update_property(&[gtk::accessible::Property::Label("Transcript")]);
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&text_view)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&clamped(&controls));
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&scrolled);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&body));
        let page = adw::NavigationPage::builder()
            .title(reader_title(&session.header))
            .child(&toolbar)
            .build();

        let reader = Rc::new(Self {
            history,
            session: Arc::new(session),
            languages,
            interrupted: status == SessionStatus::Interrupted,
            page,
            translation,
            bilingual,
            language,
            search,
            count,
            text_view,
            buffer,
            mark,
            tags,
            view: Cell::new(ExportContent::Original),
            pieces: RefCell::new(Rc::new(Vec::new())),
            fill_generation: Cell::new(0),
            filled: Cell::new(false),
            matches: RefCell::new(Vec::new()),
            current: Cell::new(0),
        });

        for toggle in [&original, &reader.translation, &reader.bilingual] {
            let weak = Rc::downgrade(&reader);
            toggle.connect_toggled(move |toggle| {
                if let Some(reader) = weak.upgrade().filter(|_| toggle.is_active()) {
                    reader.choose_view();
                }
            });
        }
        let weak = Rc::downgrade(&reader);
        reader.language.connect_selected_notify(move |_| {
            if let Some(reader) = weak.upgrade() {
                reader.choose_view();
            }
        });
        let weak = Rc::downgrade(&reader);
        copy.connect_clicked(move |_| {
            if let Some(reader) = weak.upgrade() {
                reader.copy_all();
            }
        });
        let weak = Rc::downgrade(&reader);
        export.connect_clicked(move |_| {
            if let Some(reader) = weak.upgrade()
                && let Some(history) = reader.history.upgrade()
            {
                history.open_export(reader.session.clone());
            }
        });
        let weak = Rc::downgrade(&reader);
        reader.search.connect_search_changed(move |_| {
            if let Some(reader) = weak.upgrade() {
                reader.find();
            }
        });
        reader
            .search
            .connect_stop_search(|entry| entry.set_text(""));
        let step = |forward: bool| {
            let weak = Rc::downgrade(&reader);
            move || {
                if let Some(reader) = weak.upgrade() {
                    reader.step(forward);
                }
            }
        };
        let (on_activate, on_next, on_next_match) = (step(true), step(true), step(true));
        let (on_previous, on_previous_match) = (step(false), step(false));
        reader.search.connect_activate(move |_| on_activate());
        reader.search.connect_next_match(move |_| on_next_match());
        reader
            .search
            .connect_previous_match(move |_| on_previous_match());
        next.connect_clicked(move |_| on_next());
        previous.connect_clicked(move |_| on_previous());
        reader
    }

    /// Re-renders if the selectors now ask for another view.
    fn choose_view(self: &Rc<Self>) {
        let language = self
            .languages
            .get(self.language.selected() as usize)
            .copied();
        let view = match language {
            Some(language) if self.translation.is_active() => ExportContent::Translation(language),
            Some(language) if self.bilingual.is_active() => ExportContent::Bilingual(language),
            _ => ExportContent::Original,
        };
        if view != self.view.get() {
            self.view.set(view);
            self.render();
        }
    }

    /// Fills the buffer with the current view in idle batches. A newer
    /// render or a closed reader stops a stale fill.
    fn render(self: &Rc<Self>) {
        let generation = bump(&self.fill_generation);
        self.filled.set(false);
        self.matches.borrow_mut().clear();
        self.buffer.set_text("");
        self.update_count();
        let pieces = Rc::new(transcript(&self.session, self.view.get(), self.interrupted));
        self.pieces.replace(pieces.clone());
        let weak = Rc::downgrade(self);
        let mut next = 0;
        glib::idle_add_local(move || {
            let Some(reader) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if reader.fill_generation.get() != generation {
                return glib::ControlFlow::Break;
            }
            if next == 0 {
                reader.refresh_dim_color();
            }
            let end = (next + FILL_BATCH).min(pieces.len());
            let mut iter = reader.buffer.end_iter();
            for (piece, text) in &pieces[next..end] {
                reader.insert(&mut iter, *piece, text);
            }
            next = end;
            if next < pieces.len() {
                return glib::ControlFlow::Continue;
            }
            reader.filled.set(true);
            reader.find();
            glib::ControlFlow::Break
        });
    }

    /// Dims times and notes relative to the current theme's text color.
    fn refresh_dim_color(&self) {
        let color = self.text_view.color();
        let dim = gdk::RGBA::new(
            color.red(),
            color.green(),
            color.blue(),
            color.alpha() * 0.6,
        );
        self.tags.time.set_foreground_rgba(Some(&dim));
        self.tags.note.set_foreground_rgba(Some(&dim));
    }

    fn insert(&self, iter: &mut gtk::TextIter, piece: Piece, text: &str) {
        let tags = &self.tags;
        match piece {
            Piece::Text => self.buffer.insert(iter, text),
            Piece::Time => self.buffer.insert_with_tags(iter, text, &[&tags.time]),
            Piece::Note => self.buffer.insert_with_tags(iter, text, &[&tags.note]),
            Piece::Translation => self.buffer.insert_with_tags(iter, text, &[&tags.indent]),
            Piece::MissingTranslation => {
                self.buffer
                    .insert_with_tags(iter, text, &[&tags.indent, &tags.note]);
            }
        }
    }

    fn copy_all(&self) {
        let text: String = self
            .pieces
            .borrow()
            .iter()
            .map(|(_, text)| text.as_str())
            .collect();
        self.text_view.clipboard().set_text(&text);
        if let Some(history) = self.history.upgrade() {
            history.toast("Copied");
        }
    }

    /// Highlights every match of the query in the rendered buffer and
    /// selects the first.
    fn find(&self) {
        let (start, end) = self.buffer.bounds();
        self.buffer.remove_tag(&self.tags.found, &start, &end);
        self.buffer.remove_tag(&self.tags.current, &start, &end);
        let query = self.search.text();
        let query = query.trim();
        let matches = if self.filled.get() && !query.is_empty() {
            history::find_matches(&self.buffer.text(&start, &end, false), query)
        } else {
            Vec::new()
        };
        for range in &matches {
            let (start, end) = self.range_iters(range);
            self.buffer.apply_tag(&self.tags.found, &start, &end);
        }
        self.matches.replace(matches);
        self.select(0);
    }

    /// Moves to the next or previous match, wrapping around.
    fn step(&self, forward: bool) {
        let total = self.matches.borrow().len();
        if total > 0 {
            let current = self.current.get() % total;
            self.select(if forward {
                (current + 1) % total
            } else {
                (current + total - 1) % total
            });
        }
    }

    fn select(&self, index: usize) {
        self.current.set(index);
        let range = self.matches.borrow().get(index).cloned();
        if let Some(range) = range {
            let (start, end) = self.buffer.bounds();
            self.buffer.remove_tag(&self.tags.current, &start, &end);
            let (start, end) = self.range_iters(&range);
            self.buffer.apply_tag(&self.tags.current, &start, &end);
            self.buffer.move_mark(&self.mark, &start);
            self.text_view
                .scroll_to_mark(&self.mark, 0.1, true, 0.0, 0.3);
        }
        self.update_count();
    }

    fn update_count(&self) {
        let total = self.matches.borrow().len();
        let label = if self.search.text().trim().is_empty() {
            String::new()
        } else if !self.filled.get() {
            "…".to_owned()
        } else if total == 0 {
            "No matches".to_owned()
        } else {
            format!("{} of {total}", self.current.get() + 1)
        };
        self.count.set_label(&label);
    }

    fn range_iters(&self, range: &Range<usize>) -> (gtk::TextIter, gtk::TextIter) {
        let offset = |chars: usize| i32::try_from(chars).unwrap_or(i32::MAX);
        (
            self.buffer.iter_at_offset(offset(range.start)),
            self.buffer.iter_at_offset(offset(range.end)),
        )
    }
}

/// The rendered transcript of `session` in `view`, as styled pieces.
fn transcript(
    session: &HistorySession,
    view: ExportContent,
    interrupted: bool,
) -> Vec<(Piece, String)> {
    let line = |text: &str| format!("{text}\n");
    let mut pieces = Vec::new();
    if session.header.started_late {
        pieces.push((Piece::Note, line(LATE_NOTE)));
    }
    for segment in &session.segments {
        if segment.after_gap {
            pieces.push((Piece::Note, line(GAP_NOTE)));
        }
        let time = format!("[{}] ", history::clock_time(segment.start));
        pieces.push((Piece::Time, time));
        match view {
            ExportContent::Original => pieces.push((Piece::Text, line(&segment.text))),
            ExportContent::Translation(language) => {
                pieces.push(match segment.translation(language) {
                    Some(text) => (Piece::Text, line(text)),
                    None => (Piece::Note, line(NO_TRANSLATION_NOTE)),
                })
            }
            ExportContent::Bilingual(language) => {
                pieces.push((Piece::Text, line(&segment.text)));
                pieces.push(match segment.translation(language) {
                    Some(text) => (Piece::Translation, line(text)),
                    None => (Piece::MissingTranslation, line(NO_TRANSLATION_NOTE)),
                });
            }
        }
    }
    if session.ends_with_gap {
        pieces.push((Piece::Note, line(GAP_NOTE)));
    }
    if interrupted {
        pieces.push((Piece::Note, line(INTERRUPTED_NOTE)));
    }
    pieces
}

/// Radio rows for `options`, the first chosen, and the chosen value.
fn choice_group<T: Copy + 'static>(
    title: &str,
    options: &[(T, String)],
) -> (adw::PreferencesGroup, Rc<Cell<Option<T>>>) {
    let group = adw::PreferencesGroup::builder().title(title).build();
    let chosen = Rc::new(Cell::new(options.first().map(|(value, _)| *value)));
    let mut leader: Option<gtk::CheckButton> = None;
    for (value, label) in options {
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .active(leader.is_none())
            .build();
        check.set_group(leader.as_ref());
        leader.get_or_insert_with(|| check.clone());
        let (chosen, value) = (chosen.clone(), *value);
        check.connect_toggled(move |check| {
            if check.is_active() {
                chosen.set(Some(value));
            }
        });
        let row = adw::ActionRow::builder()
            .title(label)
            .use_markup(false)
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        group.add(&row);
    }
    (group, chosen)
}

/// A flat icon-only button with a tooltip and an accessible name.
fn icon_button(icon: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// A hidden, wrapping, start-aligned note.
fn note_label(classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(classes.to_vec())
        .visible(false)
        .build()
}

fn clamped(child: &impl IsA<gtk::Widget>) -> adw::Clamp {
    adw::Clamp::builder().maximum_size(720).child(child).build()
}

fn bump(counter: &Cell<u64>) -> u64 {
    let next = counter.get().wrapping_add(1);
    counter.set(next);
    next
}

fn recording_id(status: &HistoryStatus) -> Option<&SessionId> {
    match status {
        HistoryStatus::Recording(id) => Some(id),
        _ => None,
    }
}

/// Unix milliseconds as local time in a GLib `format`; empty if invalid.
fn local_format(unix_ms: u64, format: &str) -> String {
    i64::try_from(unix_ms / 1000)
        .ok()
        .and_then(|seconds| glib::DateTime::from_unix_local(seconds).ok())
        .and_then(|time| time.format(format).ok())
        .map(String::from)
        .unwrap_or_default()
}

/// "<spoken language> · <local HH:MM>", the reader's title.
fn reader_title(header: &SessionHeader) -> String {
    format!(
        "{} · {}",
        header.spoken_language.label(),
        local_format(header.started_at_ms, "%H:%M")
    )
}

/// "<spoken language> · <source>", with the targets of a translation mode.
fn session_title(header: &SessionHeader) -> String {
    let mut title = format!(
        "{} · {}",
        header.spoken_language.label(),
        header.source.label()
    );
    if header.mode.translates() && !header.targets.is_empty() {
        let targets: Vec<&str> = header.targets.iter().map(|target| target.label()).collect();
        title.push_str(" → ");
        title.push_str(&targets.join(", "));
    }
    title
}

/// "<time> · <duration> · <segments> · <mode>", and the status unless
/// complete.
fn session_subtitle(time: &str, summary: &SessionSummary, status: SessionStatus) -> String {
    let segments = match summary.segments {
        1 => "1 segment".to_owned(),
        count => format!("{count} segments"),
    };
    let mut subtitle = format!(
        "{time} · {} · {segments} · {}",
        duration_label(summary.duration),
        summary.header.mode.label()
    );
    if status != SessionStatus::Completed {
        subtitle.push_str(" · ");
        subtitle.push_str(status.label());
    }
    subtitle
}

/// A short duration such as "45 s", "12 min" or "1 h 05 min".
fn duration_label(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3_600 => format!("{} min", seconds / 60),
        _ => format!("{} h {:02} min", seconds / 3_600, seconds / 60 % 60),
    }
}

/// "Today", "Yesterday" or `date` for the local day `day`.
fn day_heading(day: &str, today: &str, yesterday: &str, date: &str) -> String {
    if day == today {
        "Today".to_owned()
    } else if day == yesterday {
        "Yesterday".to_owned()
    } else {
        date.to_owned()
    }
}

fn library_footer(sessions: usize, bytes: u64) -> String {
    let noun = if sessions == 1 { "session" } else { "sessions" };
    format!("{sessions} {noun} · {:.1} MB", bytes as f64 / 1_000_000.0)
}

fn unreadable_label(count: usize) -> String {
    match count {
        1 => "1 saved file couldn't be read and is not shown.".to_owned(),
        count => format!("{count} saved files couldn't be read and are not shown."),
    }
}

#[cfg(test)]
mod tests {
    use lcrt_core::{Language, LanguageSelection, ProcessingMode, history::SourceKind};

    use super::*;

    fn header(mode: ProcessingMode) -> SessionHeader {
        SessionHeader {
            version: history::FORMAT_VERSION,
            started_at_ms: 1_760_000_000_000,
            mode,
            spoken_language: LanguageSelection::Language(Language::Japanese),
            targets: vec![Language::English, Language::Vietnamese],
            source: SourceKind::SystemAudio,
            started_late: false,
        }
    }

    fn segment(id: u64, text: &str, english: Option<&str>) -> history::TranscriptSegment {
        history::TranscriptSegment {
            id,
            start: Duration::from_secs(id * 61),
            end: Duration::from_secs(id * 61 + 3),
            text: text.to_owned(),
            translations: english
                .map(|text| vec![(Language::English, text.to_owned())])
                .unwrap_or_default(),
            after_gap: false,
        }
    }

    fn session() -> HistorySession {
        HistorySession {
            id: SessionId::parse("1-1").unwrap(),
            header: header(ProcessingMode::OfflineTranslation),
            segments: vec![
                segment(1, "こんにちは", Some("Hello")),
                segment(2, "はい", None),
            ],
            end: None,
            gaps: 0,
            ends_with_gap: false,
            damaged_lines: 0,
        }
    }

    fn text(pieces: &[(Piece, String)]) -> String {
        pieces.iter().map(|(_, text)| text.as_str()).collect()
    }

    #[test]
    fn titles_name_language_source_and_translation_targets() {
        assert_eq!(
            session_title(&header(ProcessingMode::OfflineCaptions)),
            "Japanese · System audio"
        );
        assert_eq!(
            session_title(&header(ProcessingMode::Translation)),
            "Japanese · System audio → English, Vietnamese"
        );
    }

    #[test]
    fn durations_are_short_and_readable() {
        assert_eq!(duration_label(Duration::from_secs(45)), "45 s");
        assert_eq!(duration_label(Duration::from_secs(12 * 60 + 59)), "12 min");
        assert_eq!(
            duration_label(Duration::from_secs(3_600 + 5 * 60)),
            "1 h 05 min"
        );
    }

    #[test]
    fn subtitles_mention_status_only_when_not_complete() {
        let summary = SessionSummary {
            id: SessionId::parse("1-1").unwrap(),
            header: header(ProcessingMode::OfflineCaptions),
            end: None,
            segments: 1,
            gaps: 0,
            duration: Duration::from_secs(90),
            size_bytes: 10,
        };
        assert_eq!(
            session_subtitle("09:05", &summary, SessionStatus::Completed),
            "09:05 · 1 min · 1 segment · Offline Captions"
        );
        assert!(
            session_subtitle("09:05", &summary, SessionStatus::Interrupted)
                .ends_with(" · Interrupted")
        );
    }

    #[test]
    fn days_are_named_relative_to_today() {
        assert_eq!(
            day_heading("2026-10-24", "2026-10-24", "2026-10-23", "x"),
            "Today"
        );
        assert_eq!(
            day_heading("2026-10-23", "2026-10-24", "2026-10-23", "x"),
            "Yesterday"
        );
        assert_eq!(
            day_heading("2026-10-20", "2026-10-24", "2026-10-23", "Tue 20 Oct 2026"),
            "Tue 20 Oct 2026"
        );
        let date = glib::DateTime::from_unix_utc(1_760_000_000).unwrap();
        assert_eq!(date.format("%a %-d %b %Y").unwrap(), "Thu 9 Oct 2025");
    }

    #[test]
    fn footer_and_unreadable_counts_are_pluralised() {
        assert_eq!(library_footer(1, 1_250_000), "1 session · 1.2 MB");
        assert_eq!(library_footer(3, 0), "3 sessions · 0.0 MB");
        assert_eq!(
            unreadable_label(1),
            "1 saved file couldn't be read and is not shown."
        );
        assert!(unreadable_label(2).starts_with("2 saved files"));
    }

    #[test]
    fn transcripts_show_each_view_and_explain_what_is_missing() {
        let mut session = session();
        assert_eq!(
            text(&transcript(&session, ExportContent::Original, false)),
            "[01:01] こんにちは\n[02:02] はい\n"
        );
        let translated = transcript(
            &session,
            ExportContent::Translation(Language::English),
            false,
        );
        assert_eq!(
            translated[3],
            (Piece::Note, format!("{NO_TRANSLATION_NOTE}\n"))
        );
        let bilingual = transcript(&session, ExportContent::Bilingual(Language::English), false);
        assert_eq!(bilingual[2], (Piece::Translation, "Hello\n".to_owned()));
        assert_eq!(bilingual[5].0, Piece::MissingTranslation);

        session.header.started_late = true;
        session.segments[1].after_gap = true;
        session.ends_with_gap = true;
        let pieces = transcript(&session, ExportContent::Original, true);
        let notes: Vec<&str> = pieces
            .iter()
            .filter(|(piece, _)| *piece == Piece::Note)
            .map(|(_, text)| text.trim_end())
            .collect();
        assert_eq!(notes, [LATE_NOTE, GAP_NOTE, GAP_NOTE, INTERRUPTED_NOTE]);
        assert_eq!(pieces[0].0, Piece::Note);
        assert_eq!(pieces[3].1, format!("{GAP_NOTE}\n"));
    }
}
