use std::{
    cell::{Cell, OnceCell, RefCell},
    rc::Rc,
    sync::mpsc::{SyncSender, TrySendError},
    thread,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use lcrt_core::{
    AudioSourceDescriptor, Language, LanguageSelection, Preferences, ProcessingMode, SessionOptions,
};
use libadwaita as adw;
use tracing::{debug, info};

use crate::{
    CaptionUiAction, CredentialView, GtkCaptionReceiver,
    captions::{CaptionViews, Vocabulary},
    preferences::{PreferencesShared, PreferencesWindow},
    presentation::{
        LanguageControl, SessionPhase, active_status, caption_css, language_control,
        preferred_source_index, privacy_notice, source_label,
    },
};

const NORMAL_APPLICATION_ID: &str = "io.github.hoangnguyen7474.Lcrt";
const DIAGNOSTIC_APPLICATION_ID: &str = "io.github.hoangnguyen7474.Lcrt.Diagnostic";
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const OVERLAY_BOTTOM_MARGIN: i32 = 36;
const ON_DEMAND_KEYBOARD_PROTOCOL_VERSION: u32 = 4;
const PLACEHOLDER: &str = "Press Start to begin live captions";

/// Initial native-window settings.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptionUiOptions {
    /// Whether the window belongs to the normal application or a diagnostic run.
    pub mode: CaptionUiMode,
    /// Prefer compositor-managed always-on-top presentation when supported.
    pub prefer_overlay: bool,
    /// PipeWire sources available for the current application session.
    pub sources: Vec<AudioSourceDescriptor>,
    /// Persisted preferences at startup.
    pub preferences: Preferences,
    /// Whether `--model` or `LCRT_MODEL_PATH` overrides the chosen model.
    pub model_overridden: bool,
}

/// GTK application identity for the current process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptionUiMode {
    /// The normal interactive LCRT application.
    Normal,
    /// A bounded diagnostic run that must not activate a normal LCRT process.
    Diagnostic,
}

impl CaptionUiMode {
    fn application_id(self) -> &'static str {
        match self {
            Self::Normal => NORMAL_APPLICATION_ID,
            Self::Diagnostic => DIAGNOSTIC_APPLICATION_ID,
        }
    }
}

impl Default for CaptionUiOptions {
    fn default() -> Self {
        Self {
            mode: CaptionUiMode::Normal,
            prefer_overlay: true,
            sources: Vec::new(),
            preferences: Preferences::default(),
            model_overridden: false,
        }
    }
}

/// Runs the GTK main loop until the user closes the window or the bridge requests exit.
pub fn run_caption_ui(
    events: GtkCaptionReceiver,
    actions: SyncSender<CaptionUiAction>,
    options: CaptionUiOptions,
) -> glib::ExitCode {
    let application_id = options.mode.application_id();
    let application = adw::Application::builder()
        .application_id(application_id)
        .build();
    application.connect_startup(|_| {
        // Matches the installed icon, for window managers that ask the window.
        gtk::Window::set_default_icon_name(NORMAL_APPLICATION_ID);
    });
    let events = Rc::new(RefCell::new(Some(events)));
    application.connect_activate(move |application| {
        if let Some(window) = application.active_window() {
            window.present();
            return;
        }
        let Some(events) = events.borrow_mut().take() else {
            return;
        };
        CaptionWindow::build(application, events, actions.clone(), &options);
    });
    application.run_with_args(&[application_id])
}

/// Dropdown items as plain labels, optionally ellipsized at the end.
fn label_factory(ellipsize: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::builder().xalign(0.0).build();
        if ellipsize {
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_width_chars(12);
        }
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let (Some(label), Some(text)) = (
            item.child().and_downcast::<gtk::Label>(),
            item.item().and_downcast::<gtk::StringObject>(),
        ) {
            label.set_text(&text.string());
        }
    });
    factory
}

fn string_list(labels: &[String]) -> gtk::StringList {
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    gtk::StringList::new(&labels)
}

struct CaptionWindow {
    window: adw::ApplicationWindow,
    preferences: Rc<RefCell<Preferences>>,
    shared: Rc<PreferencesShared>,
    preferences_window: OnceCell<PreferencesWindow>,
    sources: Vec<AudioSourceDescriptor>,
    phase: Cell<SessionPhase>,
    mode: gtk::DropDown,
    source: gtk::DropDown,
    language: gtk::DropDown,
    language_kind: Cell<LanguageControl>,
    show_original: gtk::CheckButton,
    start: gtk::Button,
    status: gtk::Label,
    notice: gtk::Label,
    error: gtk::Label,
    error_settings: gtk::Button,
    error_revealer: gtk::Revealer,
    captions: CaptionViews,
    vocabulary: OnceCell<Rc<Vocabulary>>,
    css: gtk::CssProvider,
    actions: SyncSender<CaptionUiAction>,
    last_credential: RefCell<Option<CredentialView>>,
    /// Suppresses reacting to programmatic control changes.
    updating_controls: Cell<bool>,
}

impl CaptionWindow {
    fn build(
        application: &adw::Application,
        events: GtkCaptionReceiver,
        actions: SyncSender<CaptionUiAction>,
        options: &CaptionUiOptions,
    ) {
        let preferences = Rc::new(RefCell::new(options.preferences.clone().normalized()));
        let css = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let mode = gtk::DropDown::from_strings(&ProcessingMode::ALL.map(ProcessingMode::label));
        mode.set_tooltip_text(Some("Processing mode"));
        mode.update_property(&[gtk::accessible::Property::Label("Processing mode")]);
        let source_labels: Vec<String> = options.sources.iter().map(source_label).collect();
        let source = gtk::DropDown::new(Some(string_list(&source_labels)), None::<gtk::Expression>);
        source.set_tooltip_text(Some("Audio source"));
        source.update_property(&[gtk::accessible::Property::Label("Audio source")]);
        source.set_hexpand(true);
        // A long device name must not force the caption window wider; the
        // popup list still shows full names.
        source.set_factory(Some(&label_factory(true)));
        source.set_list_factory(Some(&label_factory(false)));
        let language = gtk::DropDown::new(None::<gtk::StringList>, None::<gtk::Expression>);
        language.update_property(&[gtk::accessible::Property::Label("Language")]);
        let show_original = gtk::CheckButton::with_label("Original");
        show_original.set_tooltip_text(Some("Also show the original speech"));
        let start = gtk::Button::with_label("Start");
        start.add_css_class("suggested-action");
        start.set_tooltip_text(Some("Start or stop captions"));
        let status = gtk::Label::new(Some("Ready"));
        status.add_css_class("dim-label");
        let settings = gtk::Button::from_icon_name("emblem-system-symbolic");
        settings.set_tooltip_text(Some("Settings"));
        settings.update_property(&[gtk::accessible::Property::Label("Settings")]);

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        controls.set_margin_top(10);
        for widget in [
            mode.upcast_ref::<gtk::Widget>(),
            source.upcast_ref(),
            language.upcast_ref(),
            show_original.upcast_ref(),
            start.upcast_ref(),
            status.upcast_ref(),
            settings.upcast_ref(),
        ] {
            controls.append(widget);
        }
        let notice = gtk::Label::builder().xalign(0.0).wrap(true).build();
        notice.add_css_class("dim-label");
        notice.add_css_class("caption-notice");

        let error = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .hexpand(true)
            .build();
        error.add_css_class("error");
        let error_settings = gtk::Button::with_label("Open Settings");
        error_settings.set_valign(gtk::Align::Center);
        let error_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        error_row.append(&error);
        error_row.append(&error_settings);
        let error_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .child(&error_row)
            .build();

        let captions = CaptionViews::new(PLACEHOLDER);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_start(20);
        content.set_margin_end(20);
        content.set_margin_bottom(14);
        content.add_css_class("caption-panel");
        content.append(&error_revealer);
        content.append(&captions.root);
        content.append(&controls);
        content.append(&notice);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        let appearance = preferences.borrow().appearance.clone();
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("LCRT Live Captions")
            .default_width(appearance.width)
            .default_height(appearance.height)
            .content(&toolbar)
            .build();
        window.add_css_class("caption-overlay");
        let overlay = configure_overlay(&window, options.prefer_overlay);
        info!(overlay, "caption window created");

        let this = Rc::new_cyclic(|weak: &std::rc::Weak<CaptionWindow>| {
            let weak = weak.clone();
            let apply: Rc<dyn Fn(&Preferences)> = Rc::new(move |preferences: &Preferences| {
                if let Some(this) = weak.upgrade() {
                    this.apply_preferences(preferences);
                }
            });
            CaptionWindow {
                window: window.clone(),
                shared: PreferencesShared::new(
                    Rc::clone(&preferences),
                    actions.clone(),
                    apply,
                    options.model_overridden,
                ),
                preferences,
                preferences_window: OnceCell::new(),
                sources: options.sources.clone(),
                phase: Cell::new(SessionPhase::Idle),
                mode,
                source,
                language,
                language_kind: Cell::new(LanguageControl::None),
                show_original,
                start,
                status,
                notice,
                error,
                error_settings,
                error_revealer,
                captions,
                vocabulary: OnceCell::new(),
                css,
                actions: actions.clone(),
                last_credential: RefCell::new(None),
                updating_controls: Cell::new(false),
            }
        });

        let weak = Rc::downgrade(&this);
        let vocabulary_preferences = this.preferences.borrow().vocabulary.clone();
        let vocabulary = Vocabulary::new(
            actions.clone(),
            vocabulary_preferences.enabled,
            vocabulary_preferences.explanation_language,
            move || {
                if let Some(this) = weak.upgrade() {
                    this.open_preferences(Some("online"));
                }
            },
        );
        for view in this.captions.views() {
            vocabulary.attach(&view);
        }
        let _ = this.vocabulary.set(vocabulary);

        this.initialize_controls();
        this.apply_preferences(&this.preferences.borrow().clone());
        this.connect_controls(&settings);
        if options.sources.is_empty() {
            this.show_error(
                "No PipeWire system-audio or microphone source is available.",
                false,
            );
        }
        window.present();
        this.poll_events(events);

        // The window owns the controller state: handlers hold weak references,
        // and this strong one lives exactly as long as the window.
        let owner = Rc::clone(&this);
        let close_actions = actions;
        window.connect_close_request(move |_| {
            request_controller_shutdown(close_actions.clone(), owner.shared.take_pending());
            glib::Propagation::Proceed
        });
    }

    fn initialize_controls(self: &Rc<Self>) {
        self.updating_controls.set(true);
        let general = self.preferences.borrow().general.clone();
        let mode_index = ProcessingMode::ALL
            .iter()
            .position(|mode| *mode == general.default_mode)
            .unwrap_or_default();
        self.mode.set_selected(mode_index as u32);
        if let Some(index) =
            preferred_source_index(&self.sources, general.default_source_id.as_deref())
        {
            self.source.set_selected(index as u32);
        }
        self.show_original.set_active(general.show_original);
        self.updating_controls.set(false);
        self.refresh_mode_controls();
    }

    fn selected_mode(&self) -> ProcessingMode {
        ProcessingMode::ALL[self.mode.selected() as usize % ProcessingMode::ALL.len()]
    }

    /// Rebuilds the language choice for the selected mode.
    fn refresh_mode_controls(&self) {
        let mode = self.selected_mode();
        let kind = language_control(mode);
        self.updating_controls.set(true);
        let general = self.preferences.borrow().general.clone();
        if kind != self.language_kind.get() {
            let (labels, tooltip, selected): (Vec<String>, &str, usize) = match kind {
                LanguageControl::None => (Vec::new(), "", 0),
                LanguageControl::Spoken => (
                    std::iter::once("Auto".to_owned())
                        .chain(
                            Language::ALL
                                .iter()
                                .map(|language| language.label().to_owned()),
                        )
                        .collect(),
                    "Spoken language (Auto detects it)",
                    general
                        .spoken_language
                        .language()
                        .and_then(|language| Language::ALL.iter().position(|l| *l == language))
                        .map_or(0, |index| index + 1),
                ),
                LanguageControl::Target => (
                    Language::ALL
                        .iter()
                        .map(|language| format!("→ {}", language.label()))
                        .collect(),
                    "Translate to (the spoken language is detected automatically)",
                    Language::ALL
                        .iter()
                        .position(|language| *language == general.translation_target)
                        .unwrap_or_default(),
                ),
            };
            self.language.set_model(Some(&string_list(&labels)));
            self.language.set_selected(selected as u32);
            self.language.set_tooltip_text(Some(tooltip));
            self.language_kind.set(kind);
        }
        self.language.set_visible(kind != LanguageControl::None);
        self.show_original
            .set_visible(mode == ProcessingMode::Translation);
        self.notice.set_text(privacy_notice(mode));
        self.updating_controls.set(false);
        self.refresh_start_button();
    }

    fn refresh_start_button(&self) {
        let controls = self.phase.get().controls(!self.sources.is_empty());
        self.start.set_label(controls.start_label);
        self.start.set_sensitive(controls.start_sensitive);
        if controls.start_label == "Stop" {
            self.start.remove_css_class("suggested-action");
            self.start.add_css_class("destructive-action");
        } else {
            self.start.remove_css_class("destructive-action");
            self.start.add_css_class("suggested-action");
        }
    }

    /// The session the controls describe, remembered as the next defaults.
    fn session_options(&self) -> Option<SessionOptions> {
        let source = self.sources.get(self.source.selected() as usize)?;
        let mode = self.selected_mode();
        let selected = self.language.selected() as usize;
        let general = self.preferences.borrow().general.clone();
        let spoken_language = if self.language_kind.get() == LanguageControl::Spoken {
            match selected {
                0 => LanguageSelection::Auto,
                index => {
                    LanguageSelection::Language(Language::ALL[(index - 1) % Language::ALL.len()])
                }
            }
        } else {
            general.spoken_language
        };
        let translation_target = if self.language_kind.get() == LanguageControl::Target {
            Language::ALL[selected % Language::ALL.len()]
        } else {
            general.translation_target
        };
        Some(SessionOptions {
            mode,
            source_id: source.id().to_owned(),
            spoken_language,
            translation_target,
            show_original: self.show_original.is_active(),
        })
    }

    fn remember(&self, options: &SessionOptions) {
        let options = options.clone();
        self.shared.change(move |preferences| {
            preferences.general.default_mode = options.mode;
            preferences.general.default_source_id = Some(options.source_id);
            preferences.general.spoken_language = options.spoken_language;
            preferences.general.translation_target = options.translation_target;
            preferences.general.show_original = options.show_original;
        });
    }

    fn send(self: &Rc<Self>, action: CaptionUiAction) -> bool {
        // The controller must see pending preference changes (such as a
        // newly chosen model) before the action that depends on them.
        if !self.shared.flush() {
            self.show_error("LCRT is busy. Try again.", false);
            return false;
        }
        match self.actions.try_send(action) {
            Ok(()) => true,
            Err(_) => {
                self.show_error("LCRT is busy. Try again.", false);
                false
            }
        }
    }

    /// Reacts to a control change: remember it and, during a session,
    /// restart with the new options.
    fn controls_changed(self: &Rc<Self>) {
        if self.updating_controls.get() {
            return;
        }
        self.refresh_mode_controls();
        let Some(options) = self.session_options() else {
            return;
        };
        self.remember(&options);
        if matches!(
            self.phase.get(),
            SessionPhase::Starting | SessionPhase::Running
        ) && self.send(CaptionUiAction::Start(options))
        {
            self.set_phase(SessionPhase::Starting);
        }
    }

    fn set_phase(&self, phase: SessionPhase) {
        self.phase.set(phase);
        self.refresh_start_button();
    }

    fn connect_controls(self: &Rc<Self>, settings: &gtk::Button) {
        for dropdown in [&self.mode, &self.source, &self.language] {
            let weak = Rc::downgrade(self);
            dropdown.connect_selected_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.controls_changed();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.show_original.connect_toggled(move |_| {
            if let Some(this) = weak.upgrade() {
                this.controls_changed();
            }
        });
        let weak = Rc::downgrade(self);
        self.start.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            match this.phase.get() {
                SessionPhase::Idle => {
                    let Some(options) = this.session_options() else {
                        this.show_error("Select an audio source before starting captions.", false);
                        return;
                    };
                    this.remember(&options);
                    this.hide_error();
                    if this.send(CaptionUiAction::Start(options)) {
                        this.set_phase(SessionPhase::Starting);
                        this.status.set_text("Starting…");
                    }
                }
                SessionPhase::Starting | SessionPhase::Running => {
                    if this.send(CaptionUiAction::Stop) {
                        this.set_phase(SessionPhase::Stopping);
                        this.status.set_text("Stopping…");
                    }
                }
                SessionPhase::Stopping => {}
            }
        });
        let weak = Rc::downgrade(self);
        settings.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.open_preferences(None);
            }
        });
        let weak = Rc::downgrade(self);
        self.error_settings.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let page = if this.selected_mode().streams_audio_online() {
                    "online"
                } else {
                    "general"
                };
                this.open_preferences(Some(page));
            }
        });
    }

    fn open_preferences(self: &Rc<Self>, page: Option<&str>) {
        let preferences = self
            .preferences_window
            .get_or_init(|| PreferencesWindow::new(&self.window, &self.shared));
        if let Some(view) = self.last_credential.borrow().as_ref() {
            preferences.set_credential(view);
        }
        match page {
            Some(page) => preferences.show_page(page),
            None => preferences.present(),
        }
    }

    fn apply_preferences(&self, preferences: &Preferences) {
        let appearance = &preferences.appearance;
        self.css.load_from_data(&caption_css(appearance));
        self.window
            .set_default_size(appearance.width, appearance.height);
        if let Some(vocabulary) = self.vocabulary.get() {
            vocabulary.configure(
                preferences.vocabulary.enabled,
                preferences.vocabulary.explanation_language,
            );
        }
    }

    fn show_error(&self, message: &str, needs_settings: bool) {
        self.error.set_text(message);
        self.error_settings.set_visible(needs_settings);
        self.error_revealer.set_reveal_child(true);
    }

    fn hide_error(&self) {
        self.error_revealer.set_reveal_child(false);
    }

    fn poll_events(self: &Rc<Self>, events: GtkCaptionReceiver) {
        let weak = Rc::downgrade(self);
        let application = self.window.application();
        glib::timeout_add_local(EVENT_POLL_INTERVAL, move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let update = match events.take_update() {
                Ok(update) => update,
                Err(error) => {
                    this.show_error(&format!("Caption UI bridge failed: {error}"), false);
                    this.set_phase(SessionPhase::Idle);
                    if let Some(application) = &application {
                        application.quit();
                    }
                    return glib::ControlFlow::Break;
                }
            };
            if let Some(presentation) = update.presentation {
                if let Some(snapshot) = presentation.caption {
                    debug!(
                        revision = snapshot.revision(),
                        ui_state_age_us = snapshot.age().as_micros(),
                        "caption update reached GTK"
                    );
                    let show_original = this.show_original.is_active();
                    this.captions.show(
                        snapshot.caption().text(),
                        snapshot.caption().original(),
                        show_original,
                    );
                }
                if let Some(running) = presentation.running {
                    if running {
                        this.captions.reset("");
                        if this.phase.get() != SessionPhase::Stopping {
                            this.set_phase(SessionPhase::Running);
                        }
                    } else {
                        this.set_phase(SessionPhase::Idle);
                    }
                }
                if let Some(status) = presentation.status {
                    let status = if status == "Listening…" {
                        active_status(this.selected_mode()).to_owned()
                    } else {
                        status
                    };
                    this.status.set_text(&status);
                }
            }
            if let Some(error) = update.error {
                match error {
                    Some(message) => this.show_error(&message, update.error_needs_settings),
                    None => this.hide_error(),
                }
            }
            if let Some(view) = update.credential {
                if let Some(preferences) = this.preferences_window.get() {
                    preferences.set_credential(&view);
                }
                *this.last_credential.borrow_mut() = Some(view);
            }
            if let (Some(outcome), Some(vocabulary)) = (update.vocabulary, this.vocabulary.get()) {
                vocabulary.deliver(outcome);
            }
            if update.quit {
                if let Some(application) = &application {
                    application.quit();
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}

/// Delivers a pending preference change, then Shutdown, in that order. A
/// full queue must not lose either, so the rest is sent from a helper thread.
fn request_controller_shutdown(
    actions: SyncSender<CaptionUiAction>,
    pending: Option<Box<Preferences>>,
) {
    let mut queue: std::collections::VecDeque<CaptionUiAction> = pending
        .map(CaptionUiAction::SavePreferences)
        .into_iter()
        .chain([CaptionUiAction::Shutdown])
        .collect();
    while let Some(action) = queue.pop_front() {
        match actions.try_send(action) {
            Ok(()) => {}
            Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(action)) => {
                queue.push_front(action);
                thread::spawn(move || {
                    for action in queue {
                        if actions.send(action).is_err() {
                            return;
                        }
                    }
                });
                return;
            }
        }
    }
}

fn configure_overlay(window: &adw::ApplicationWindow, prefer_overlay: bool) -> bool {
    let wayland_display = gdk::Display::default()
        .is_some_and(|display| display.type_().name() == "GdkWaylandDisplay");
    let protocol_version = if prefer_overlay && wayland_display {
        gtk4_layer_shell::protocol_version()
    } else {
        0
    };
    let layer_shell_active =
        overlay_protocol_is_usable(prefer_overlay, wayland_display, protocol_version);
    if !layer_shell_active {
        info!(
            prefer_overlay,
            wayland_display,
            protocol_version,
            "interactive layer-shell unavailable; using compositor-managed standard window"
        );
        return false;
    }

    window.init_layer_shell();
    window.set_namespace(Some("lcrt-caption-overlay"));
    window.set_layer(Layer::Overlay);
    window.set_anchor(Edge::Bottom, true);
    window.set_margin(Edge::Bottom, OVERLAY_BOTTOM_MARGIN);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    window.set_exclusive_zone(0);
    info!(protocol_version, "layer-shell overlay presentation enabled");
    true
}

fn overlay_protocol_is_usable(
    prefer_overlay: bool,
    wayland_display: bool,
    protocol_version: u32,
) -> bool {
    prefer_overlay && wayland_display && protocol_version >= ON_DEMAND_KEYBOARD_PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc::sync_channel, time::Duration};

    use lcrt_core::Preferences;

    use super::{
        CaptionUiMode, CaptionUiOptions, DIAGNOSTIC_APPLICATION_ID, NORMAL_APPLICATION_ID,
        overlay_protocol_is_usable, request_controller_shutdown,
    };
    use crate::CaptionUiAction;

    #[test]
    fn closing_delivers_a_pending_preference_change_before_shutdown_even_when_busy() {
        let (actions, received) = sync_channel(1);
        actions.try_send(CaptionUiAction::Stop).unwrap(); // the queue is full
        let mut preferences = Preferences::default();
        preferences.appearance.width = 999;
        request_controller_shutdown(actions, Some(Box::new(preferences.clone())));
        let timeout = Duration::from_secs(5);
        assert_eq!(
            received.recv_timeout(timeout).unwrap(),
            CaptionUiAction::Stop
        );
        assert_eq!(
            received.recv_timeout(timeout).unwrap(),
            CaptionUiAction::SavePreferences(Box::new(preferences))
        );
        assert_eq!(
            received.recv_timeout(timeout).unwrap(),
            CaptionUiAction::Shutdown
        );
    }

    #[test]
    fn overlay_is_preferred_by_default() {
        assert!(CaptionUiOptions::default().prefer_overlay);
    }

    #[test]
    fn overlay_requires_wayland_and_on_demand_keyboard_protocol() {
        assert!(overlay_protocol_is_usable(true, true, 4));
        assert!(!overlay_protocol_is_usable(true, true, 3));
        assert!(!overlay_protocol_is_usable(true, false, 4));
        assert!(!overlay_protocol_is_usable(false, true, 4));
    }

    #[test]
    fn diagnostics_use_a_distinct_application_identity() {
        assert_eq!(
            CaptionUiMode::Normal.application_id(),
            NORMAL_APPLICATION_ID
        );
        assert_eq!(
            CaptionUiMode::Diagnostic.application_id(),
            DIAGNOSTIC_APPLICATION_ID
        );
    }
}
