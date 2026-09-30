//! Selectable caption text and the vocabulary popover.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc::SyncSender,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use lcrt_core::Language;

use crate::{CaptionUiAction, VocabularyCard, VocabularyOutcome, VocabularyProblem};

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

/// The original-language and caption text views.
pub(crate) struct CaptionViews {
    pub(crate) root: gtk::Box,
    original: gtk::TextView,
    caption: gtk::TextView,
    scroller: gtk::ScrolledWindow,
}

impl CaptionViews {
    pub(crate) fn new(placeholder: &str) -> Self {
        let original = caption_view("caption-original", "Original speech");
        original.set_visible(false);
        let caption = caption_view("caption-text", "Captions");
        caption.buffer().set_text(placeholder);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&caption)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_margin_top(12);
        root.append(&original);
        root.append(&scroller);
        Self {
            root,
            original,
            caption,
            scroller,
        }
    }

    /// Shows new caption text and, for translations, the original lane.
    pub(crate) fn show(&self, text: &str, original: Option<&str>, show_original: bool) {
        let original = original.filter(|text| show_original && !text.trim().is_empty());
        self.original.set_visible(original.is_some());
        if let Some(original) = original {
            replace_text(&self.original.buffer(), original);
        }
        replace_text(&self.caption.buffer(), text);
        if !self.caption.buffer().has_selection() {
            // Keep the newest words in view while the user is not selecting.
            let adjustment = self.scroller.vadjustment();
            adjustment.set_value(adjustment.upper());
        }
    }

    /// Clears both lanes, showing `placeholder` as the caption.
    pub(crate) fn reset(&self, placeholder: &str) {
        self.original.buffer().set_text("");
        self.original.set_visible(false);
        self.caption.buffer().set_text(placeholder);
    }

    pub(crate) fn views(&self) -> [gtk::TextView; 2] {
        [self.original.clone(), self.caption.clone()]
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
