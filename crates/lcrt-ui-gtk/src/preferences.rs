//! The native Preferences window.

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{SyncSender, TrySendError},
    time::Duration,
};

use gtk::{gdk, gio, glib, pango, prelude::*};
use lcrt_core::{
    AppearancePreferences, GeneralPreferences, Language, LanguageSelection, Preferences, Rgb,
    preferences::{FONT_SIZE_RANGE, HEIGHT_RANGE, WIDTH_RANGE},
};
use libadwaita::{self as adw, prelude::*};

use crate::{CaptionUiAction, CredentialTone, CredentialView, EnteredApiKey};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Shared state between the caption window and Preferences.
pub(crate) struct PreferencesShared {
    pub(crate) preferences: Rc<RefCell<Preferences>>,
    pub(crate) actions: SyncSender<CaptionUiAction>,
    /// Applies appearance and vocabulary changes to the caption window.
    pub(crate) apply: Rc<dyn Fn(&Preferences)>,
    /// Whether `--model` or `LCRT_MODEL_PATH` overrides the chosen model.
    pub(crate) model_overridden: bool,
    pending_save: RefCell<Option<glib::SourceId>>,
}

impl PreferencesShared {
    pub(crate) fn new(
        preferences: Rc<RefCell<Preferences>>,
        actions: SyncSender<CaptionUiAction>,
        apply: Rc<dyn Fn(&Preferences)>,
        model_overridden: bool,
    ) -> Rc<Self> {
        Rc::new(Self {
            preferences,
            actions,
            apply,
            model_overridden,
            pending_save: RefCell::new(None),
        })
    }

    /// Changes preferences, applies them now, and saves them shortly after.
    pub(crate) fn change(self: &Rc<Self>, change: impl FnOnce(&mut Preferences)) {
        {
            let mut preferences = self.preferences.borrow_mut();
            change(&mut preferences);
            let normalized = preferences.clone().normalized();
            *preferences = normalized;
        }
        (self.apply)(&self.preferences.borrow());
        self.schedule_save();
    }

    /// Hands any pending change to the controller now, so the action that
    /// follows (such as Start) sees it. Returns false if the controller is
    /// busy; the save is then retried shortly.
    pub(crate) fn flush(self: &Rc<Self>) -> bool {
        match self.take_pending() {
            Some(snapshot) => self.send_snapshot(snapshot),
            None => true,
        }
    }

    /// Cancels the pending save and returns its snapshot, for a caller that
    /// delivers it itself (window close).
    pub(crate) fn take_pending(&self) -> Option<Box<Preferences>> {
        let source = self.pending_save.borrow_mut().take()?;
        source.remove();
        Some(Box::new(self.preferences.borrow().clone()))
    }

    fn send_snapshot(self: &Rc<Self>, snapshot: Box<Preferences>) -> bool {
        match self
            .actions
            .try_send(CaptionUiAction::SavePreferences(snapshot))
        {
            Ok(()) | Err(TrySendError::Disconnected(_)) => true,
            Err(TrySendError::Full(_)) => {
                self.schedule_save();
                false
            }
        }
    }

    fn schedule_save(self: &Rc<Self>) {
        if let Some(source) = self.pending_save.borrow_mut().take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(SAVE_DEBOUNCE, move || {
            if let Some(shared) = weak.upgrade() {
                shared.pending_save.borrow_mut().take();
                let snapshot = Box::new(shared.preferences.borrow().clone());
                shared.send_snapshot(snapshot);
            }
        });
        *self.pending_save.borrow_mut() = Some(source);
    }
}

fn rgba(color: Rgb) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(color.red) / 255.0,
        f32::from(color.green) / 255.0,
        f32::from(color.blue) / 255.0,
        1.0,
    )
}

fn rgb(color: &gdk::RGBA) -> Rgb {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb::new(
        channel(color.red()),
        channel(color.green()),
        channel(color.blue()),
    )
}

fn language_model() -> gtk::StringList {
    let labels: Vec<&str> = Language::ALL
        .iter()
        .map(|language| language.label())
        .collect();
    gtk::StringList::new(&labels)
}

fn language_index(language: Language) -> u32 {
    Language::ALL
        .iter()
        .position(|candidate| *candidate == language)
        .unwrap_or_default() as u32
}

/// The Preferences window plus the widgets updated from outside it.
pub(crate) struct PreferencesWindow {
    window: adw::PreferencesWindow,
    credential_status: adw::ActionRow,
    /// Disabled while a connection test is in flight.
    test_connection: gtk::Button,
    translation: Rc<TranslationRows>,
}

impl PreferencesWindow {
    pub(crate) fn new(parent: &impl IsA<gtk::Window>, shared: &Rc<PreferencesShared>) -> Self {
        let window = adw::PreferencesWindow::builder()
            .title("Preferences")
            .transient_for(parent)
            .modal(false)
            .hide_on_close(true)
            .search_enabled(false)
            .default_width(620)
            .default_height(640)
            .build();
        let credential_status = adw::ActionRow::builder()
            .title("Status")
            .subtitle("Checking…")
            .build();
        let (general, translation) = general_page(&window, shared);
        window.add(&general);
        let test_connection = gtk::Button::with_label("Test connection");
        window.add(&online_page(shared, &credential_status, &test_connection));
        window.add(&appearance_page(shared));
        window.add(&vocabulary_page(shared));
        window.add(&about_page());
        Self {
            window,
            credential_status,
            test_connection,
            translation,
        }
    }

    /// Shows translation-lane settings that were changed in the caption
    /// window.
    pub(crate) fn sync_translation(&self, general: &GeneralPreferences) {
        self.translation.sync(general);
    }

    pub(crate) fn present(&self) {
        self.window.present();
    }

    pub(crate) fn show_page(&self, name: &str) {
        self.window.set_visible_page_name(name);
        self.window.present();
    }

    pub(crate) fn set_credential(&self, view: &CredentialView) {
        let marker = match view.tone {
            CredentialTone::Good => "✓ ",
            CredentialTone::Problem => "⚠ ",
            CredentialTone::Neutral => "",
        };
        self.credential_status
            .set_subtitle(&format!("{marker}{}", view.status));
        // Every credential action ends with a status, including a test.
        self.test_connection.set_sensitive(true);
    }
}

fn general_page(
    window: &adw::PreferencesWindow,
    shared: &Rc<PreferencesShared>,
) -> (adw::PreferencesPage, Rc<TranslationRows>) {
    let page = adw::PreferencesPage::builder()
        .title("General")
        .name("general")
        .icon_name("preferences-system-symbolic")
        .build();
    page.add(&offline_group(window, shared));

    let translation = TranslationRows::new(shared);
    page.add(&translation.group);

    let remembered = adw::PreferencesGroup::builder()
        .title("Session")
        .description(
            "LILOPOP remembers the mode, audio source, and languages you last used in the \
             caption window. System audio is preferred when no source was chosen.",
        )
        .build();
    page.add(&remembered);
    (page, translation)
}

/// Offline Captions use the built-in model. A custom model is optional and
/// kept under Advanced, so no one has to know what a model file is.
fn offline_group(
    window: &adw::PreferencesWindow,
    shared: &Rc<PreferencesShared>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Offline Captions")
        .description(
            "Speech is recognized on this device and audio never leaves it. Choose the spoken \
             language, or Auto, next to Start.",
        )
        .build();
    let model = adw::ActionRow::builder().title("Offline model").build();
    let custom = adw::SwitchRow::builder()
        .title("Use a custom Whisper model")
        .subtitle("Language support depends on the custom model.")
        .build();
    let file = adw::ActionRow::builder()
        .title("Model file")
        .subtitle_selectable(true)
        .build();
    let choose = gtk::Button::builder()
        .label("Choose file…")
        .valign(gtk::Align::Center)
        .build();
    file.add_suffix(&choose);
    let advanced = adw::ExpanderRow::builder().title("Advanced").build();
    advanced.add_row(&custom);
    advanced.add_row(&file);
    group.add(&model);
    group.add(&advanced);

    let show = {
        let (model, custom, file) = (model.clone(), custom.clone(), file.clone());
        let overridden = shared.model_overridden;
        move |path: Option<&PathBuf>| {
            model.set_subtitle(match (overridden, path) {
                (true, _) => "Set by --model or LCRT_MODEL_PATH for this run",
                (false, Some(_)) => "Custom Whisper model",
                (false, None) => "Fast — built-in multilingual model",
            });
            custom.set_active(path.is_some());
            file.set_visible(path.is_some());
            file.set_subtitle(&path.map_or_else(String::new, |path| path.display().to_string()));
        }
    };
    show(shared.preferences.borrow().general.custom_model.as_ref());
    custom.set_sensitive(!shared.model_overridden);
    choose.set_sensitive(!shared.model_overridden);

    let pick = {
        let (shared, parent, custom) = (Rc::clone(shared), window.clone(), custom.clone());
        let show = show.clone();
        move || {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Whisper models (*.bin)"));
            filter.add_pattern("*.bin");
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let dialog = gtk::FileDialog::builder()
                .title("Choose a Whisper model")
                .filters(&filters)
                .modal(true)
                .build();
            let (shared, custom, show) = (Rc::clone(&shared), custom.clone(), show.clone());
            dialog.open(Some(&parent), None::<&gio::Cancellable>, move |result| {
                // No file I/O here on the GTK thread: starting Offline
                // Captions loads the model and reports one that can't be read.
                match result.ok().and_then(|file| file.path()) {
                    Some(path) => {
                        shared.change(|preferences| {
                            preferences.general.custom_model = Some(path);
                        });
                    }
                    // Cancelled, or not a local file: keep what was there.
                    None => custom
                        .set_active(shared.preferences.borrow().general.custom_model.is_some()),
                }
                show(shared.preferences.borrow().general.custom_model.as_ref());
            });
        }
    };
    let pick_for_switch = pick.clone();
    let shared_for_switch = Rc::clone(shared);
    custom.connect_active_notify(move |custom| {
        let chosen = shared_for_switch
            .preferences
            .borrow()
            .general
            .custom_model
            .is_some();
        if custom.is_active() && !chosen {
            pick_for_switch();
        } else if !custom.is_active() && chosen {
            shared_for_switch.change(|preferences| preferences.general.custom_model = None);
            show(None);
        }
    });
    choose.connect_clicked(move |_| pick());
    group
}

/// Translation's settings. The lanes themselves are chosen with the chips
/// in the caption window, while captions run; only the spoken language,
/// which labels the source lane, is set here.
pub(crate) struct TranslationRows {
    group: adw::PreferencesGroup,
    spoken: adw::ComboRow,
    /// Set while the row is being refreshed, so that doesn't count as a
    /// change by the user.
    syncing: Rc<Cell<bool>>,
}

impl TranslationRows {
    fn new(shared: &Rc<PreferencesShared>) -> Rc<Self> {
        let labels: Vec<&str> = std::iter::once("Detect automatically")
            .chain(Language::ALL.iter().map(|language| language.label()))
            .collect();
        let group = adw::PreferencesGroup::builder()
            .title("Translation")
            .description(
                "Choose lanes with the language chips next to Start, also while captions run: \
                 click a chip to show or hide its lane, open its menu to pause or remove a \
                 language, and use + to add one. Up to two translation languages; each is its \
                 own session, and API charges apply for each.",
            )
            .build();
        let spoken = adw::ComboRow::builder()
            .title("Spoken language")
            .subtitle(
                "Names the original lane, and no lane translates into it. Translation detects \
                 the language itself.",
            )
            .model(&gtk::StringList::new(&labels))
            .build();
        group.add(&spoken);
        let rows = Rc::new(Self {
            group,
            spoken,
            syncing: Rc::new(Cell::new(false)),
        });
        rows.sync(&shared.preferences.borrow().general);
        let (weak, shared) = (Rc::downgrade(&rows), Rc::clone(shared));
        rows.spoken.connect_selected_notify(move |row| {
            let Some(rows) = weak.upgrade() else {
                return;
            };
            if rows.syncing.get() {
                return;
            }
            let spoken = (row.selected() as usize)
                .checked_sub(1)
                .and_then(|index| Language::ALL.get(index).copied())
                .map_or(LanguageSelection::Auto, LanguageSelection::Language);
            shared.change(|preferences| {
                preferences.general.spoken_language = spoken;
                // A target that now repeats the spoken language is dropped.
                let targets = preferences.general.translation_targets();
                preferences
                    .general
                    .set_translation_targets(Some(targets.first()), targets.second());
            });
        });
        rows
    }

    /// Makes the row show `general`.
    pub(crate) fn sync(&self, general: &GeneralPreferences) {
        let index = general
            .spoken_language
            .language()
            .and_then(|language| Language::ALL.iter().position(|l| *l == language))
            .map_or(0, |index| index as u32 + 1);
        self.syncing.set(true);
        self.spoken.set_selected(index);
        self.syncing.set(false);
    }
}

fn online_page(
    shared: &Rc<PreferencesShared>,
    status: &adw::ActionRow,
    test: &gtk::Button,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Online")
        .name("online")
        .icon_name("network-wireless-symbolic")
        .build();
    let group = adw::PreferencesGroup::builder()
        .title("OpenAI")
        .description(
            "Online Captions, Translation, and vocabulary explanations use your own OpenAI \
             API key. Audio is streamed to OpenAI for processing. API charges may apply to \
             your OpenAI account. The key is kept in the system keyring, never in a file.",
        )
        .build();
    let key_row = adw::PasswordEntryRow::builder().title("API key").build();
    let test = test.clone();
    let save = gtk::Button::with_label("Save securely");
    save.add_css_class("suggested-action");
    let clear = gtk::Button::with_label("Clear");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(12);
    buttons.append(&test);
    buttons.append(&clear);
    buttons.append(&save);

    const BUSY: &str = "⚠ LILOPOP is busy. Try again.";
    let actions = shared.actions.clone();
    let entry = key_row.clone();
    let row = status.clone();
    save.connect_clicked(move |_| {
        let action = CaptionUiAction::SaveApiKey(EnteredApiKey::new(entry.text().to_string()));
        if actions.try_send(action).is_ok() {
            // Clear the plaintext entry only once the key is handed off.
            entry.set_text("");
        } else {
            row.set_subtitle(BUSY);
        }
    });
    let actions = shared.actions.clone();
    let row = status.clone();
    let entry = key_row.clone();
    test.connect_clicked(move |button| {
        let text = entry.text();
        let entered = (!text.trim().is_empty()).then(|| EnteredApiKey::new(text.to_string()));
        if actions
            .try_send(CaptionUiAction::TestConnection(entered))
            .is_ok()
        {
            row.set_subtitle("Testing…");
            button.set_sensitive(false);
        } else {
            row.set_subtitle(BUSY);
        }
    });
    let actions = shared.actions.clone();
    let row = status.clone();
    clear.connect_clicked(move |_| {
        if actions.try_send(CaptionUiAction::ClearApiKey).is_err() {
            row.set_subtitle(BUSY);
        }
    });

    group.add(&key_row);
    group.add(status);
    group.add(&buttons);
    page.add(&group);
    page
}

/// A titled row with a numeric field. `adw::SpinRow` is not exposed to
/// assistive technologies (libadwaita 1.9), so this composes a plain
/// `gtk::SpinButton`, which is.
fn number_row(title: &str, range: (f64, f64), step: f64) -> (adw::ActionRow, gtk::SpinButton) {
    let spin = gtk::SpinButton::with_range(range.0, range.1, step);
    spin.set_valign(gtk::Align::Center);
    spin.update_property(&[gtk::accessible::Property::Label(title)]);
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable_widget(&spin)
        .build();
    row.add_suffix(&spin);
    (row, spin)
}

fn appearance_page(shared: &Rc<PreferencesShared>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Appearance")
        .name("appearance")
        .icon_name("applications-graphics-symbolic")
        .build();
    let group = adw::PreferencesGroup::builder()
        .title("Captions")
        .description("Changes preview immediately and are remembered.")
        .build();
    let appearance = shared.preferences.borrow().appearance.clone();

    let system_font = adw::SwitchRow::builder()
        .title("Use system font")
        .active(appearance.font_family.is_none())
        .build();
    let font_dialog = gtk::FontDialog::builder().title("Caption font").build();
    let font_button = gtk::FontDialogButton::builder()
        .dialog(&font_dialog)
        .level(gtk::FontLevel::Family)
        .valign(gtk::Align::Center)
        .sensitive(appearance.font_family.is_some())
        .build();
    if let Some(family) = &appearance.font_family {
        font_button.set_font_desc(&pango::FontDescription::from_string(family));
    }
    let font_row = adw::ActionRow::builder().title("Font family").build();
    font_row.add_suffix(&font_button);

    font_button.update_property(&[gtk::accessible::Property::Label("Caption font")]);
    let (size_row, size) = number_row("Font size (pt)", FONT_SIZE_RANGE, 1.0);
    let text_color = gtk::ColorDialogButton::builder()
        .dialog(&gtk::ColorDialog::builder().with_alpha(false).build())
        .valign(gtk::Align::Center)
        .build();
    text_color.update_property(&[gtk::accessible::Property::Label("Text color")]);
    let text_row = adw::ActionRow::builder().title("Text color").build();
    text_row.add_suffix(&text_color);
    let background_color = gtk::ColorDialogButton::builder()
        .dialog(&gtk::ColorDialog::builder().with_alpha(false).build())
        .valign(gtk::Align::Center)
        .build();
    background_color.update_property(&[gtk::accessible::Property::Label("Background color")]);
    let background_row = adw::ActionRow::builder().title("Background color").build();
    background_row.add_suffix(&background_color);
    let (opacity_row, opacity) = number_row("Background opacity (%)", (0.0, 100.0), 5.0);
    let (width_row, width) = number_row(
        "Caption window width",
        (f64::from(WIDTH_RANGE.0), f64::from(WIDTH_RANGE.1)),
        20.0,
    );
    let (height_row, height) = number_row(
        "Caption window height",
        (f64::from(HEIGHT_RANGE.0), f64::from(HEIGHT_RANGE.1)),
        20.0,
    );
    let reset = gtk::Button::builder()
        .label("Reset appearance to defaults")
        .halign(gtk::Align::End)
        .margin_top(12)
        .build();

    let fill = {
        let (size, text_color, background_color, opacity, width, height) = (
            size.clone(),
            text_color.clone(),
            background_color.clone(),
            opacity.clone(),
            width.clone(),
            height.clone(),
        );
        let (system_font, font_button) = (system_font.clone(), font_button.clone());
        move |appearance: &AppearancePreferences| {
            system_font.set_active(appearance.font_family.is_none());
            font_button.set_sensitive(appearance.font_family.is_some());
            size.set_value(appearance.font_size_points);
            text_color.set_rgba(&rgba(appearance.text_color));
            background_color.set_rgba(&rgba(appearance.background_color));
            opacity.set_value((appearance.background_opacity * 100.0).round());
            width.set_value(f64::from(appearance.width));
            height.set_value(f64::from(appearance.height));
        }
    };
    fill(&appearance);

    let change = |shared: &Rc<PreferencesShared>| Rc::clone(shared);
    let s = change(shared);
    let button = font_button.clone();
    system_font.connect_active_notify(move |row| {
        let use_system = row.is_active();
        button.set_sensitive(!use_system);
        let family = (!use_system)
            .then(|| {
                button
                    .font_desc()
                    .and_then(|desc| desc.family().map(|f| f.to_string()))
            })
            .flatten();
        s.change(|preferences| preferences.appearance.font_family = family);
    });
    let s = change(shared);
    font_button.connect_font_desc_notify(move |button| {
        let family = button
            .font_desc()
            .and_then(|desc| desc.family().map(|family| family.to_string()));
        if family.is_some() {
            s.change(|preferences| preferences.appearance.font_family = family);
        }
    });
    let s = change(shared);
    size.connect_value_notify(move |row| {
        let value = row.value();
        s.change(|preferences| preferences.appearance.font_size_points = value);
    });
    let s = change(shared);
    text_color.connect_rgba_notify(move |button| {
        let color = rgb(&button.rgba());
        s.change(|preferences| preferences.appearance.text_color = color);
    });
    let s = change(shared);
    background_color.connect_rgba_notify(move |button| {
        let color = rgb(&button.rgba());
        s.change(|preferences| preferences.appearance.background_color = color);
    });
    let s = change(shared);
    opacity.connect_value_notify(move |row| {
        let value = row.value() / 100.0;
        s.change(|preferences| preferences.appearance.background_opacity = value);
    });
    let s = change(shared);
    width.connect_value_notify(move |row| {
        let value = row.value().round() as i32;
        s.change(|preferences| preferences.appearance.width = value);
    });
    let s = change(shared);
    height.connect_value_notify(move |row| {
        let value = row.value().round() as i32;
        s.change(|preferences| preferences.appearance.height = value);
    });
    let s = change(shared);
    reset.connect_clicked(move |_| {
        // Credentials and other preferences are untouched.
        s.change(|preferences| preferences.appearance = AppearancePreferences::default());
        fill(&AppearancePreferences::default());
    });

    for row in [
        system_font.upcast_ref::<gtk::Widget>(),
        font_row.upcast_ref(),
        size_row.upcast_ref(),
        text_row.upcast_ref(),
        background_row.upcast_ref(),
        opacity_row.upcast_ref(),
        width_row.upcast_ref(),
        height_row.upcast_ref(),
    ] {
        group.add(row);
    }
    group.add(&reset);
    page.add(&group);
    page
}

fn vocabulary_page(shared: &Rc<PreferencesShared>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Vocabulary")
        .name("vocabulary")
        .icon_name("accessories-dictionary-symbolic")
        .build();
    let vocabulary = shared.preferences.borrow().vocabulary.clone();
    let group = adw::PreferencesGroup::builder()
        .title("Vocabulary explanations")
        .description(
            "Select a word or phrase in the captions to see what it means in context. \
             Only the selection and a little surrounding caption text are sent to OpenAI, \
             and only when you select text.",
        )
        .build();
    let enabled = adw::SwitchRow::builder()
        .title("Explain selected words")
        .active(vocabulary.enabled)
        .build();
    let language = adw::ComboRow::builder()
        .title("Explain in")
        .model(&language_model())
        .selected(language_index(vocabulary.explanation_language))
        .build();
    let s = Rc::clone(shared);
    enabled.connect_active_notify(move |row| {
        let active = row.is_active();
        s.change(|preferences| preferences.vocabulary.enabled = active);
    });
    let s = Rc::clone(shared);
    language.connect_selected_notify(move |row| {
        let language = Language::ALL[row.selected() as usize % Language::ALL.len()];
        s.change(|preferences| preferences.vocabulary.explanation_language = language);
    });
    group.add(&enabled);
    group.add(&language);
    page.add(&group);
    page
}

fn about_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Privacy")
        .name("privacy")
        .icon_name("security-high-symbolic")
        .build();
    let group = adw::PreferencesGroup::builder()
        .title(format!("LILOPOP {}", env!("CARGO_PKG_VERSION")))
        .description(
            "Translate and caption, live — no connection needed.\n\
             Offline Captions and Offline Translation: audio is processed on this device.\n\
             Online Captions and Online Translation: audio is streamed to OpenAI for processing, \
             only while such a session is running. API charges may apply to your OpenAI \
             account.\n\
             Vocabulary (online modes only): the selected text and a little surrounding caption text are sent to \
             OpenAI when you select text.\n\
             LILOPOP has no telemetry or analytics and never stores recordings or transcripts.",
        )
        .build();
    page.add(&group);
    page
}
