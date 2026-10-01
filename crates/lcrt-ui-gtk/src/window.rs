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
    AudioSourceDescriptor, Language, LanguageSelection, Preferences, ProcessingMode,
    SessionOptions, TargetChange, TargetStatus, TranslationTargets,
};
use libadwaita as adw;
use tracing::{debug, info};

use crate::{
    CaptionUiAction, CredentialView, GtkCaptionReceiver,
    captions::{CaptionViews, Vocabulary},
    lane_controls::{LaneAction, LaneControls},
    preferences::{PreferencesShared, PreferencesWindow},
    presentation::{
        SessionPhase, active_status, caption_css, lane_layout, preferred_source_index,
        privacy_notice, source_label, translation_status,
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
    /// The spoken language of Offline and Online Captions.
    language: gtk::DropDown,
    /// The lane chips of Translation, in place of the language dropdown.
    lanes: OnceCell<Rc<LaneControls>>,
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
    /// The options of the running session, with its targets as they are
    /// after live changes. They tell whether a change needs a restart, and
    /// they lay out the rows while the session runs.
    session: RefCell<Option<SessionOptions>>,
    /// What each target of the running session reported last.
    target_statuses: RefCell<Vec<(Language, TargetStatus)>>,
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
        let language_labels: Vec<String> = std::iter::once("Auto".to_owned())
            .chain(
                Language::ALL
                    .iter()
                    .map(|language| language.label().to_owned()),
            )
            .collect();
        let language =
            gtk::DropDown::new(Some(string_list(&language_labels)), None::<gtk::Expression>);
        language.set_tooltip_text(Some("Spoken language (Auto detects it)"));
        language.update_property(&[gtk::accessible::Property::Label("Language")]);
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
                lanes: OnceCell::new(),
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
                session: RefCell::new(None),
                target_statuses: RefCell::new(Vec::new()),
            }
        });

        let weak = Rc::downgrade(&this);
        let lanes = LaneControls::new(move |action| {
            if let Some(this) = weak.upgrade() {
                this.lane_action(action);
            }
        });
        controls.insert_child_after(&lanes.root, Some(&this.language));
        let _ = this.lanes.set(lanes);

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
        self.updating_controls.set(false);
        self.sync_language();
        self.refresh_mode_controls();
    }

    fn selected_mode(&self) -> ProcessingMode {
        ProcessingMode::ALL[self.mode.selected() as usize % ProcessingMode::ALL.len()]
    }

    /// Shows the language choice of the selected mode: the spoken language
    /// for captions, the lane chips for Translation.
    fn refresh_mode_controls(&self) {
        let translation = self.selected_mode() == ProcessingMode::Translation;
        self.language.set_visible(!translation);
        if let Some(lanes) = self.lanes.get() {
            lanes.root.set_visible(translation);
        }
        self.refresh_lanes();
        self.refresh_start_button();
    }

    /// Whether a session is starting or running, so live changes reach it.
    fn session_running(&self) -> bool {
        matches!(
            self.phase.get(),
            SessionPhase::Starting | SessionPhase::Running
        )
    }

    /// The targets on screen: the running session's, else the stored ones.
    fn current_targets(&self) -> TranslationTargets {
        match (self.session_running(), self.session.borrow().as_ref()) {
            (true, Some(session)) => session.translation_targets,
            _ => self.preferences.borrow().general.translation_targets(),
        }
    }

    /// Lays out the caption rows and lane chips, and shows the notice, for
    /// the running session or what the controls describe.
    fn refresh_lanes(&self) {
        let general = self.preferences.borrow().general.clone();
        let options = match (self.session_running(), self.session.borrow().clone()) {
            // Translation detects the language itself, so naming it in
            // Settings relabels the source lane without a restart.
            (true, Some(mut session)) => {
                if session.mode == ProcessingMode::Translation {
                    session.spoken_language = general.spoken_language;
                }
                session
            }
            _ => self.described_options(),
        };
        let statuses = self.target_statuses.borrow().clone();
        let layout = lane_layout(
            options.mode,
            options.spoken_language,
            options.translation_targets,
            &general,
            &statuses,
        );
        self.notice.set_text(privacy_notice(
            options.mode,
            options.translation_targets.iter().count(),
        ));
        // While idle, text left from the last session is cleared when the
        // rows now show other languages, or captions instead of lanes.
        if self.captions.configure(&layout) && self.phase.get() == SessionPhase::Idle {
            self.captions.reset(PLACEHOLDER);
        }
        if let Some(lanes) = self.lanes.get() {
            lanes.update(
                &layout,
                &options
                    .translation_targets
                    .addable(options.spoken_language.language()),
                self.session_running(),
            );
        }
    }

    /// Makes the language dropdown show the stored spoken language, after it
    /// was changed in Settings.
    fn sync_language(&self) {
        let spoken = self.preferences.borrow().general.spoken_language;
        let index = spoken
            .language()
            .and_then(|language| Language::ALL.iter().position(|l| *l == language))
            .map_or(0, |index| index + 1);
        self.updating_controls.set(true);
        self.language.set_selected(index as u32);
        self.updating_controls.set(false);
    }

    /// Carries out a lane chip's action at once. Showing and hiding change
    /// only what is shown; target changes reach a running session as
    /// [`TargetChange`], which opens or closes that target's session only.
    fn lane_action(self: &Rc<Self>, action: LaneAction) {
        let running = self.session_running();
        let spoken = self.preferences.borrow().general.spoken_language.language();
        let current = self.current_targets();
        let change = match action {
            LaneAction::Toggle(lane) => {
                let visible = self.preferences.borrow().general.lane_visible(lane);
                self.shared.change(|preferences| {
                    preferences.general.set_lane_visible(lane, !visible);
                });
                return;
            }
            LaneAction::Add(language) => current
                .with_added(language, spoken)
                .map(|targets| (targets, TargetChange::Add(language))),
            LaneAction::Remove(language) => current
                .without(language)
                .map(|targets| (targets, TargetChange::Remove(language))),
            LaneAction::Pause(language) if running => {
                self.send_target_change(TargetChange::Pause(language), Some(TargetStatus::Paused));
                return;
            }
            LaneAction::Resume(language) if running => {
                self.send_target_change(
                    TargetChange::Resume(language),
                    Some(TargetStatus::Connecting),
                );
                return;
            }
            LaneAction::Pause(_) | LaneAction::Resume(_) => return,
        };
        let Some((targets, change)) = change else {
            return;
        };
        if running {
            if let Some(session) = self.session.borrow_mut().as_mut() {
                session.translation_targets = targets;
            }
            let expected =
                matches!(change, TargetChange::Add(_)).then_some(TargetStatus::Connecting);
            self.send_target_change(change, expected);
        }
        self.shared.change(|preferences| {
            preferences
                .general
                .set_translation_targets(Some(targets.first()), targets.second());
        });
    }

    /// Sends a live target change and shows `expected` on its lane until
    /// the session reports otherwise; a removed lane has none.
    fn send_target_change(self: &Rc<Self>, change: TargetChange, expected: Option<TargetStatus>) {
        let (TargetChange::Add(language)
        | TargetChange::Remove(language)
        | TargetChange::Pause(language)
        | TargetChange::Resume(language)) = change;
        if !self.send(CaptionUiAction::ChangeTarget(change)) {
            return;
        }
        let mut statuses = self.target_statuses.borrow_mut();
        statuses.retain(|(target, _)| *target != language);
        statuses.extend(expected.map(|status| (language, status)));
        drop(statuses);
        self.refresh_lanes();
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
        (!self.sources.is_empty()).then(|| self.described_options())
    }

    /// What the controls and settings describe, even with no audio source.
    fn described_options(&self) -> SessionOptions {
        let source_id = self
            .sources
            .get(self.source.selected() as usize)
            .map(|source| source.id().to_owned())
            .unwrap_or_default();
        let mode = self.selected_mode();
        let general = self.preferences.borrow().general.clone();
        // Translation has no language dropdown; it names the spoken language
        // in Settings, only to label the source lane.
        let spoken_language = if mode == ProcessingMode::Translation {
            general.spoken_language
        } else {
            match self.language.selected() as usize {
                0 => LanguageSelection::Auto,
                index => {
                    LanguageSelection::Language(Language::ALL[(index - 1) % Language::ALL.len()])
                }
            }
        };
        SessionOptions {
            mode,
            source_id,
            spoken_language,
            translation_targets: general.translation_targets(),
        }
    }

    fn remember(&self, options: &SessionOptions) {
        let options = options.clone();
        self.shared.change(move |preferences| {
            preferences.general.default_mode = options.mode;
            preferences.general.default_source_id = Some(options.source_id);
            preferences.general.spoken_language = options.spoken_language;
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
        self.restart_if_changed();
    }

    /// Restarts a running session when the controls changed what its
    /// backend depends on: the mode, the audio source, or the spoken
    /// language of captions. Everything else changes live. The controller
    /// replaces the session only after the old one has fully stopped.
    fn restart_if_changed(self: &Rc<Self>) {
        let Some(mut options) = self.session_options() else {
            return;
        };
        let needs_restart = self
            .session
            .borrow()
            .as_ref()
            .is_none_or(|session| session.needs_restart_for(&options));
        if self.session_running() && needs_restart {
            // The replacement keeps the targets the running session has.
            if let Some(session) = self.session.borrow().as_ref() {
                options.translation_targets = session.translation_targets;
            }
            if self.send(CaptionUiAction::Start(options.clone())) {
                *self.session.borrow_mut() = Some(options);
                self.set_phase(SessionPhase::Starting);
            }
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
                    if this.send(CaptionUiAction::Start(options.clone())) {
                        *this.session.borrow_mut() = Some(options);
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

    fn apply_preferences(self: &Rc<Self>, preferences: &Preferences) {
        let appearance = &preferences.appearance;
        self.css.load_from_data(&caption_css(appearance));
        self.window
            .set_default_size(appearance.width, appearance.height);
        self.sync_language();
        self.refresh_lanes();
        if let Some(window) = self.preferences_window.get() {
            window.sync_translation(&preferences.general);
        }
        // Settings can change the lanes of a running session. Restart it
        // once this change has been stored, not from inside it.
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(this) = weak.upgrade() {
                this.restart_if_changed();
            }
        });
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
                // A new session's reset must come before any caption that was
                // coalesced into the same update, or it would erase it.
                // Lanes are laid out again only when they changed, not for
                // every caption, so an open chip menu stays open.
                let lanes_changed = presentation.started.is_some()
                    || presentation.session_targets.is_some()
                    || presentation.running.is_some()
                    || !presentation.targets.is_empty();
                if let Some(options) = presentation.started {
                    // The controller says what actually started.
                    *this.session.borrow_mut() = Some(options);
                }
                if let Some(targets) = presentation.session_targets
                    && let Some(session) = this.session.borrow_mut().as_mut()
                {
                    // ...and which targets it has after a live change.
                    session.translation_targets = targets;
                }
                if let Some(running) = presentation.running {
                    if running {
                        // A session starts with empty rows laid out for it.
                        this.captions.reset("");
                        this.target_statuses.borrow_mut().clear();
                        if this.phase.get() != SessionPhase::Stopping {
                            this.set_phase(SessionPhase::Running);
                        }
                    } else {
                        this.set_phase(SessionPhase::Idle);
                        this.target_statuses.borrow_mut().clear();
                    }
                }
                {
                    let mut statuses = this.target_statuses.borrow_mut();
                    for (language, status) in &presentation.targets {
                        statuses.retain(|(target, _)| target != language);
                        statuses.push((*language, *status));
                    }
                }
                if lanes_changed {
                    this.refresh_lanes();
                }
                if let Some(snapshot) = presentation.caption {
                    debug!(
                        revision = snapshot.revision(),
                        ui_state_age_us = snapshot.age().as_micros(),
                        "caption update reached GTK"
                    );
                    this.captions.show(snapshot.caption());
                }
                if let Some(status) = presentation.status {
                    let status = if status == "Listening…" {
                        active_status(this.selected_mode()).to_owned()
                    } else {
                        status
                    };
                    this.status.set_text(&status);
                }
                // A translation's status follows its targets' own.
                if this.phase.get() == SessionPhase::Running
                    && !presentation.targets.is_empty()
                    && let Some(status) = translation_status(&this.target_statuses.borrow())
                {
                    this.status.set_text(status);
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
