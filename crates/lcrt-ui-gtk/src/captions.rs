//! Selectable caption text and the vocabulary popover.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc::SyncSender,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use lcrt_core::Language;

use crate::{
    CaptionUiAction, VocabularyCard, VocabularyOutcome, VocabularyProblem,
    presentation::{LaneLayout, row_fit},
};

/// How long a selection must stay unchanged before it is explained.
const SELECTION_SETTLE: Duration = Duration::from_millis(500);

/// Number of leading characters two texts share.
pub(crate) fn common_prefix_chars(current: &str, next: &str) -> usize {
    current
        .chars()
        .zip(next.chars())
        .take_while(|(left, right)| left == right)
        .count()
}

/// Replaces the buffer text by editing only the differing suffix, so a
/// selection inside unchanged text survives live caption updates.
fn replace_text(buffer: &gtk::TextBuffer, text: &str) {
    let current = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
    if current == text {
        return;
    }
    let shared = common_prefix_chars(&current, text);
    let mut start = buffer.iter_at_offset(i32::try_from(shared).unwrap_or(i32::MAX));
    let mut end = buffer.end_iter();
    buffer.delete(&mut start, &mut end);
    let suffix: String = text.chars().skip(shared).collect();
    let mut insert_at = buffer.end_iter();
    buffer.insert(&mut insert_at, &suffix);
}

fn caption_view(css_class: &str, accessible_label: &str) -> gtk::TextView {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .justification(gtk::Justification::Center)
        .hexpand(true)
        .build();
    view.add_css_class(css_class);
    view.update_property(&[gtk::accessible::Property::Label(accessible_label)]);
    view
}

/// One caption row: a language badge and its selectable text.
struct Lane {
    row: gtk::Box,
    badge: gtk::Label,
    /// Clips the badge, so neither it nor its offset from the top ever makes
    /// the row taller than the height it is given.
    badge_holder: gtk::ScrolledWindow,
    view: gtk::TextView,
    scroller: gtk::ScrolledWindow,
    /// Whether the row has a badge, and therefore fits its text to its
    /// height.
    labeled: Rc<Cell<bool>>,
}

/// Fits a labeled row's text to its height.
///
/// With room for two lines the text wraps; with less it is one unwrapped
/// line that follows the newest words, so the row is always full rather than
/// showing the last word of a wrapped paragraph. Only whole lines are shown:
/// the height left over goes above the text, and the badge moves down with it
/// to stay level with the first line.
fn fit_lines(
    view: &gtk::TextView,
    scroller: &gtk::ScrolledWindow,
    badge: &gtk::Label,
    labeled: bool,
) {
    let margin = scroller.margin_top();
    let available = scroller.vadjustment().page_size() as i32 + margin;
    let line = view.iter_location(&view.buffer().start_iter()).height();
    let fit = row_fit(labeled, available, line);
    if fit.single_line != (view.wrap_mode() == gtk::WrapMode::None) {
        if fit.single_line {
            // Let the line overflow sideways first, or unwrapping it would
            // ask for the whole line's width.
            scroller.set_hscrollbar_policy(gtk::PolicyType::External);
            view.set_wrap_mode(gtk::WrapMode::None);
        } else {
            view.set_wrap_mode(gtk::WrapMode::WordChar);
            scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
        }
    }
    if fit.top_gap != margin {
        scroller.set_margin_top(fit.top_gap);
        badge.set_margin_top(fit.top_gap);
    }
}

impl Lane {
    fn new(css_class: &str, accessible_label: &str) -> Self {
        let badge = gtk::Label::builder().valign(gtk::Align::Start).build();
        badge.add_css_class("lane-badge");
        let badge_holder = gtk::ScrolledWindow::builder()
            .child(&badge)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .propagate_natural_height(true)
            .visible(false)
            .build();
        badge_holder.add_css_class("lane-badge-holder");
        let view = caption_view(css_class, accessible_label);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        scroller.add_css_class("caption-lane");
        // Keep the newest words in view, down and sideways, while the user
        // is not selecting.
        for adjustment in [scroller.vadjustment(), scroller.hadjustment()] {
            let text = view.downgrade();
            adjustment.connect_changed(move |adjustment| {
                if text
                    .upgrade()
                    .is_some_and(|view| !view.buffer().has_selection())
                {
                    adjustment.set_value(adjustment.upper() - adjustment.page_size());
                }
            });
        }
        let labeled = Rc::new(Cell::new(false));
        {
            // The row's height or the text's height (a new font size)
            // changed: fit the text to the row once this layout pass is
            // over. One pending fit is enough however often that happens.
            let widgets = (view.downgrade(), scroller.downgrade(), badge.downgrade());
            let labeled = Rc::clone(&labeled);
            let pending = Rc::new(Cell::new(false));
            scroller.vadjustment().connect_changed(move |_| {
                if pending.replace(true) {
                    return;
                }
                let (widgets, labeled, pending) =
                    (widgets.clone(), Rc::clone(&labeled), Rc::clone(&pending));
                glib::idle_add_local_once(move || {
                    pending.set(false);
                    if let (Some(view), Some(scroller), Some(badge)) = (
                        widgets.0.upgrade(),
                        widgets.1.upgrade(),
                        widgets.2.upgrade(),
                    ) {
                        fit_lines(&view, &scroller, &badge, labeled.get());
                    }
                });
            });
        }
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(&badge_holder);
        row.append(&scroller);
        Self {
            row,
            badge,
            badge_holder,
            view,
            scroller,
            labeled,
        }
    }

    /// Shows the row with `badge`, or without one when `badge` is `None`.
    /// A row with a badge takes whatever height it is given, fits its text
    /// to it, and shows no scrollbar.
    fn configure(&self, visible: bool, badge: Option<&str>) {
        self.row.set_visible(visible);
        let labeled = badge.is_some();
        self.labeled.set(labeled);
        self.scroller.set_vscrollbar_policy(if labeled {
            gtk::PolicyType::External
        } else {
            gtk::PolicyType::Automatic
        });
        self.badge_holder.set_visible(labeled);
        if let Some(badge) = badge {
            self.badge.set_text(badge);
        }
        // Rows with a badge read as a list; a single row stays centered.
        self.view.set_justification(if labeled {
            gtk::Justification::Left
        } else {
            gtk::Justification::Center
        });
        fit_lines(&self.view, &self.scroller, &self.badge, labeled);
    }

    fn set_text(&self, text: &str) {
        replace_text(&self.view.buffer(), text);
    }
}

/// The caption rows, always in the same order: the original speech, the
/// captions (or first translation), and the second translation.
pub(crate) struct CaptionViews {
    pub(crate) root: gtk::Box,
    source: Lane,
    first: Lane,
    second: Lane,
    layout: RefCell<LaneLayout>,
}

impl CaptionViews {
    pub(crate) fn new(placeholder: &str) -> Self {
        let source = Lane::new("caption-original", "Original speech");
        let first = Lane::new("caption-text", "Captions");
        let second = Lane::new("caption-text", "Second translation");
        first.view.buffer().set_text(placeholder);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_margin_top(12);
        root.set_margin_start(14);
        root.set_margin_end(14);
        // Visible rows share the height equally, so a row never moves or
        // resizes because another one received text.
        root.set_homogeneous(true);
        for lane in [&source, &first, &second] {
            root.append(&lane.row);
        }
        let views = Self {
            root,
            source,
            first,
            second,
            layout: RefCell::new(LaneLayout::default()),
        };
        views.configure(&LaneLayout::default());
        views
    }

    /// Shows the rows `layout` describes and returns whether that changed
    /// which rows or badges are shown. Row order never changes.
    pub(crate) fn configure(&self, layout: &LaneLayout) -> bool {
        let changed = *self.layout.borrow() != *layout;
        self.source
            .configure(layout.source.is_some(), layout.source.as_deref());
        self.first.configure(true, layout.first.as_deref());
        self.second
            .configure(layout.second.is_some(), layout.second.as_deref());
        *self.layout.borrow_mut() = layout.clone();
        changed
    }

    /// Updates each row's text. A row without new text keeps what it shows.
    pub(crate) fn show(&self, text: &str, original: Option<&str>, second: Option<&str>) {
        if let Some(original) = original {
            self.source.set_text(original);
        }
        self.first.set_text(text);
        if let Some(second) = second {
            self.second.set_text(second);
        }
    }

    /// Clears every row, showing `placeholder` in the first.
    pub(crate) fn reset(&self, placeholder: &str) {
        self.source.view.buffer().set_text("");
        self.second.view.buffer().set_text("");
        self.first.view.buffer().set_text(placeholder);
    }

    pub(crate) fn views(&self) -> [gtk::TextView; 3] {
        [
            self.source.view.clone(),
            self.first.view.clone(),
            self.second.view.clone(),
        ]
    }
}

/// Selection-driven vocabulary explanations.
pub(crate) struct Vocabulary {
    popover: gtk::Popover,
    term: gtk::Label,
    details: gtk::Label,
    meaning: gtk::Label,
    context: gtk::Label,
    spinner: gtk::Spinner,
    copy: gtk::Button,
    settings: gtk::Button,
    next_request: Cell<u64>,
    active_request: Cell<u64>,
    /// The text the active request explains, as shown in the popover.
    requested_text: RefCell<String>,
    pending: RefCell<Option<glib::SourceId>>,
    /// Every caption row whose selection is watched.
    views: RefCell<Vec<glib::WeakRef<gtk::TextView>>>,
    enabled: Cell<bool>,
    language: Cell<Language>,
    actions: SyncSender<CaptionUiAction>,
}

impl Vocabulary {
    pub(crate) fn new(
        actions: SyncSender<CaptionUiAction>,
        enabled: bool,
        language: Language,
        open_settings: impl Fn() + 'static,
    ) -> Rc<Self> {
        let label = |css: Option<&str>| {
            let label = gtk::Label::builder()
                .wrap(true)
                .xalign(0.0)
                .max_width_chars(42)
                .selectable(true)
                .build();
            if let Some(css) = css {
                label.add_css_class(css);
            }
            label
        };
        let term = label(Some("title-4"));
        let details = label(Some("dim-label"));
        let meaning = label(None);
        let context = label(None);
        let spinner = gtk::Spinner::new();
        let copy = gtk::Button::with_label("Copy");
        let settings = gtk::Button::with_label("Open Settings");
        settings.connect_clicked(move |_| open_settings());
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&settings);
        buttons.append(&copy);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        for child in [
            term.upcast_ref::<gtk::Widget>(),
            details.upcast_ref(),
            spinner.upcast_ref(),
            meaning.upcast_ref(),
            context.upcast_ref(),
            buttons.upcast_ref(),
        ] {
            content.append(child);
        }
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);
        let popover = gtk::Popover::builder()
            .child(&content)
            .autohide(true)
            .build();
        let vocabulary = Rc::new(Self {
            popover,
            term,
            details,
            meaning,
            context,
            spinner,
            copy,
            settings,
            next_request: Cell::new(0),
            active_request: Cell::new(0),
            requested_text: RefCell::new(String::new()),
            pending: RefCell::new(None),
            views: RefCell::new(Vec::new()),
            enabled: Cell::new(enabled),
            language: Cell::new(language),
            actions,
        });
        let weak = Rc::downgrade(&vocabulary);
        vocabulary.popover.connect_closed(move |_| {
            // A result arriving after the popup closed must not reopen it.
            if let Some(vocabulary) = weak.upgrade() {
                vocabulary.active_request.set(0);
            }
        });
        let weak = Rc::downgrade(&vocabulary);
        vocabulary.copy.connect_clicked(move |button| {
            if let Some(vocabulary) = weak.upgrade() {
                let text = [
                    vocabulary.term.text(),
                    vocabulary.meaning.text(),
                    vocabulary.context.text(),
                ]
                .iter()
                .filter(|part| !part.is_empty())
                .map(|part| part.as_str())
                .collect::<Vec<_>>()
                .join("\n");
                button.clipboard().set_text(&text);
            }
        });
        vocabulary
    }

    pub(crate) fn configure(&self, enabled: bool, language: Language) {
        self.enabled.set(enabled);
        self.language.set(language);
        if !enabled {
            // With Vocabulary off, nothing may be sent: cancel a selection
            // still settling and retire any lookup on screen.
            if let Some(source) = self.pending.borrow_mut().take() {
                source.remove();
            }
            self.active_request.set(0);
            self.popover.popdown();
        }
    }

    /// Watches a caption view's selection.
    pub(crate) fn attach(self: &Rc<Self>, view: &gtk::TextView) {
        self.views.borrow_mut().push(view.downgrade());
        let weak = Rc::downgrade(self);
        let target = view.downgrade();
        view.buffer().connect_mark_set(move |_, _, mark| {
            let name = mark.name();
            if !matches!(name.as_deref(), Some("insert" | "selection_bound")) {
                return;
            }
            if let (Some(vocabulary), Some(view)) = (weak.upgrade(), target.upgrade()) {
                vocabulary.selection_changed(&view);
            }
        });
    }

    fn selection_changed(self: &Rc<Self>, view: &gtk::TextView) {
        if let Some(source) = self.pending.borrow_mut().take() {
            source.remove();
        }
        // Selecting different text retires the explanation on screen, so a
        // late answer can't appear next to text it doesn't explain. A
        // collapsed selection keeps it: the popover names its own term.
        let buffer = view.buffer();
        if self.active_request.get() != 0
            && let Some((start, end)) = buffer.selection_bounds()
            && buffer.text(&start, &end, false).trim() != self.requested_text.borrow().as_str()
        {
            self.active_request.set(0);
            self.popover.popdown();
        }
        if !self.enabled.get() || !view.buffer().has_selection() {
            return;
        }
        // One selection at a time across the rows, so it is always clear
        // which text is being explained. Each row is explained from its own
        // text only.
        for other in self
            .views
            .borrow()
            .iter()
            .filter_map(glib::WeakRef::upgrade)
        {
            let buffer = other.buffer();
            if other != *view && buffer.has_selection() {
                let cursor = buffer.iter_at_mark(&buffer.get_insert());
                buffer.place_cursor(&cursor);
            }
        }
        let weak = Rc::downgrade(self);
        let target = view.downgrade();
        let source = glib::timeout_add_local_once(SELECTION_SETTLE, move || {
            if let (Some(vocabulary), Some(view)) = (weak.upgrade(), target.upgrade()) {
                vocabulary.pending.borrow_mut().take();
                vocabulary.explain(&view);
            }
        });
        *self.pending.borrow_mut() = Some(source);
    }

    fn explain(&self, view: &gtk::TextView) {
        if !self.enabled.get() {
            return;
        }
        let buffer = view.buffer();
        let Some((start, end)) = buffer.selection_bounds() else {
            return;
        };
        let request_id = self.next_request.get() + 1;
        self.next_request.set(request_id);
        self.active_request.set(request_id);
        let caption = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let action = CaptionUiAction::ExplainSelection {
            request_id,
            caption: caption.to_string(),
            start: usize::try_from(start.offset()).unwrap_or_default(),
            end: usize::try_from(end.offset()).unwrap_or_default(),
            language: self.language.get(),
        };
        let selection = buffer.text(&start, &end, false);
        *self.requested_text.borrow_mut() = selection.trim().to_owned();
        self.show_loading(view, &end, &selection);
        if self.actions.try_send(action).is_err() {
            self.show_problem(&VocabularyProblem {
                message: "LCRT is busy. Try selecting the text again.".to_owned(),
                needs_settings: false,
            });
        }
    }

    fn show_loading(&self, view: &gtk::TextView, anchor: &gtk::TextIter, selection: &str) {
        if self.popover.parent().as_ref() != Some(view.upcast_ref::<gtk::Widget>()) {
            if self.popover.parent().is_some() {
                self.popover.unparent();
            }
            self.popover.set_parent(view);
        }
        let location = view.iter_location(anchor);
        let (x, y) =
            view.buffer_to_window_coords(gtk::TextWindowType::Widget, location.x(), location.y());
        self.popover.set_pointing_to(Some(&gdk::Rectangle::new(
            x,
            y,
            1,
            location.height().max(1),
        )));
        self.term.set_text(selection.trim());
        self.details.set_text("");
        self.details.set_visible(false);
        self.meaning.set_text("Looking up…");
        self.context.set_text("");
        self.context.set_visible(false);
        self.spinner.set_visible(true);
        self.spinner.start();
        self.copy.set_visible(false);
        self.settings.set_visible(false);
        self.popover.popup();
    }

    /// Shows a result if it answers the request still on screen.
    pub(crate) fn deliver(&self, outcome: VocabularyOutcome) {
        if outcome.request_id != self.active_request.get() || !self.popover.is_visible() {
            return;
        }
        match outcome.result {
            Ok(card) => self.show_card(&card),
            Err(problem) => self.show_problem(&problem),
        }
    }

    fn show_card(&self, card: &VocabularyCard) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.term.set_text(&card.term);
        let details = [card.reading.as_deref(), card.part_of_speech.as_deref()]
            .into_iter()
            .flatten()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        self.details.set_visible(!details.is_empty());
        self.details.set_text(&details);
        self.meaning.set_text(&card.meaning);
        self.context
            .set_visible(!card.context_explanation.is_empty());
        self.context.set_text(&card.context_explanation);
        self.copy.set_visible(true);
        // The content changed size. A popover's parent must present it again
        // when that happens, and a text view doesn't for a popover it didn't
        // create; without this GTK closes the popover instead of resizing it.
        self.popover.present();
    }

    fn show_problem(&self, problem: &VocabularyProblem) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.meaning.set_text(&problem.message);
        self.settings.set_visible(problem.needs_settings);
        self.popover.popup();
    }
}

#[cfg(test)]
mod tests {
    use super::common_prefix_chars;

    #[test]
    fn prefix_is_counted_in_characters_across_scripts() {
        assert_eq!(common_prefix_chars("Hello world", "Hello there"), 6);
        assert_eq!(common_prefix_chars("今日は新しい", "今日は古い"), 3);
        assert_eq!(common_prefix_chars("dự án mới", "dự án cũ"), 6);
        assert_eq!(common_prefix_chars("", "anything"), 0);
        assert_eq!(common_prefix_chars("same", "same"), 4);
    }
}
