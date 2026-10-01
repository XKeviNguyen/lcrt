//! The translation lane strip: one chip per lane and an add button.
//!
//! Clicking a chip shows or hides its lane, which is presentation only.
//! Each chip's menu pauses, resumes or removes its target, and the add
//! button adds a target, all while captions keep running.

use std::rc::Rc;

use gtk::{gio, glib, prelude::*};
use lcrt_core::{CaptionLane, Language, TargetStatus};
use libadwaita as adw;

use crate::presentation::{LaneLayout, LaneRow};

/// What the user asked the strip to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaneAction {
    /// Show a hidden lane or hide a shown one.
    Toggle(CaptionLane),
    Pause(Language),
    Resume(Language),
    Remove(Language),
    Add(Language),
}

/// One lane's chip. It is kept while its lane exists, so keyboard focus
/// stays on it as its state changes.
struct Chip {
    lane: CaptionLane,
    button: adw::SplitButton,
    label: gtk::Label,
    paused: gtk::Image,
}

pub(crate) struct LaneControls {
    pub(crate) root: gtk::Box,
    chips: std::cell::RefCell<Vec<Chip>>,
    add: gtk::MenuButton,
    on_action: Rc<dyn Fn(LaneAction)>,
}

/// The parameter naming a lane in the strip's actions.
fn lane_parameter(lane: CaptionLane) -> String {
    match lane {
        CaptionLane::Source => "source".to_owned(),
        CaptionLane::Target(language) => language.code().to_owned(),
    }
}

/// The action `name` asks for on `lane`; only showing and hiding apply to
/// the source lane.
fn lane_action(name: &str, lane: CaptionLane) -> Option<LaneAction> {
    match (name, lane) {
        ("toggle", lane) => Some(LaneAction::Toggle(lane)),
        ("pause", CaptionLane::Target(language)) => Some(LaneAction::Pause(language)),
        ("resume", CaptionLane::Target(language)) => Some(LaneAction::Resume(language)),
        ("remove", CaptionLane::Target(language)) => Some(LaneAction::Remove(language)),
        ("add", CaptionLane::Target(language)) => Some(LaneAction::Add(language)),
        _ => None,
    }
}

fn parse_lane(parameter: &str) -> Option<CaptionLane> {
    match parameter {
        "source" => Some(CaptionLane::Source),
        code => Language::from_code(code).map(CaptionLane::Target),
    }
}

impl LaneControls {
    pub(crate) fn new(on_action: impl Fn(LaneAction) + 'static) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        root.update_property(&[gtk::accessible::Property::Label("Caption lanes")]);
        let add = gtk::MenuButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add a translation language")
            .build();
        add.update_property(&[gtk::accessible::Property::Label(
            "Add a translation language",
        )]);
        root.append(&add);
        let controls = Rc::new(Self {
            root,
            chips: std::cell::RefCell::new(Vec::new()),
            add,
            on_action: Rc::new(on_action),
        });
        controls.install_actions();
        controls
    }

    /// The `lane.*` actions the chip menus and the add menu invoke.
    fn install_actions(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        for name in ["toggle", "pause", "resume", "remove", "add"] {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let on_action = Rc::clone(&self.on_action);
            action.connect_activate(move |_, parameter| {
                if let Some(action) = parameter
                    .and_then(glib::Variant::str)
                    .and_then(parse_lane)
                    .and_then(|lane| lane_action(name, lane))
                {
                    on_action(action);
                }
            });
            group.add_action(&action);
        }
        self.root.insert_action_group("lane", Some(&group));
    }

    /// Shows a chip for every lane of `layout`, in its order, then the add
    /// button with the languages that can be added. `running` enables
    /// pausing and resuming, which only a running session can do; without
    /// `editable` (while a session starts) targets can't be added or
    /// removed either.
    pub(crate) fn update(
        &self,
        layout: &LaneLayout,
        addable: &[Language],
        running: bool,
        editable: bool,
    ) {
        let lanes: Vec<(CaptionLane, &LaneRow)> = layout
            .source
            .iter()
            .map(|row| (CaptionLane::Source, row))
            .chain(
                layout
                    .targets
                    .iter()
                    .map(|(language, row)| (CaptionLane::Target(*language), row)),
            )
            .collect();
        let mut chips = self.chips.borrow_mut();
        // Chips of lanes that are gone go; the others are kept for focus.
        chips.retain(|chip| {
            let keep = lanes.iter().any(|(lane, _)| *lane == chip.lane);
            if !keep {
                self.root.remove(&chip.button);
            }
            keep
        });
        let visible = lanes.iter().filter(|(_, row)| row.visible).count();
        // A translation session always keeps one target.
        let removable = editable && layout.targets.len() > 1;
        let mut previous: Option<gtk::Widget> = None;
        for (lane, row) in &lanes {
            let index = match chips.iter().position(|chip| chip.lane == *lane) {
                Some(index) => index,
                None => {
                    chips.push(self.chip(*lane));
                    chips.len() - 1
                }
            };
            let chip = &chips[index];
            self.root
                .reorder_child_after(&chip.button, previous.as_ref());
            previous = Some(chip.button.clone().upcast());
            present(chip, row, visible == 1 && row.visible, removable, running);
        }
        self.root.reorder_child_after(&self.add, previous.as_ref());
        let menu = gio::Menu::new();
        for language in addable {
            menu.append(
                Some(language.label()),
                Some(&format!("lane.add::{}", language.code())),
            );
        }
        self.add.set_menu_model(Some(&menu));
        let full = addable.is_empty();
        self.add.set_sensitive(editable && !full);
        self.add.set_tooltip_text(Some(if !editable {
            "Languages can be added once captions have started."
        } else if full {
            "Maximum 2 translation languages."
        } else {
            "Add a translation language"
        }));
    }

    fn chip(&self, lane: CaptionLane) -> Chip {
        let label = gtk::Label::new(None);
        let paused = gtk::Image::from_icon_name("media-playback-pause-symbolic");
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        content.append(&label);
        content.append(&paused);
        let button = adw::SplitButton::builder().child(&content).build();
        button.add_css_class("lane-chip");
        let on_action = Rc::clone(&self.on_action);
        button.connect_clicked(move |_| on_action(LaneAction::Toggle(lane)));
        self.root.append(&button);
        Chip {
            lane,
            button,
            label,
            paused,
        }
    }
}

/// Shows one lane's state on its chip and in its menu.
fn present(chip: &Chip, row: &LaneRow, last_visible: bool, removable: bool, running: bool) {
    chip.label.set_text(&row.badge);
    let paused = matches!(
        row.status,
        Some(TargetStatus::Paused | TargetStatus::Failed)
    );
    chip.paused.set_visible(paused);
    if row.visible {
        chip.button.remove_css_class("hidden-lane");
    } else {
        chip.button.add_css_class("hidden-lane");
    }
    let name = match chip.lane {
        CaptionLane::Source => "Original speech".to_owned(),
        CaptionLane::Target(language) => language.label().to_owned(),
    };
    let tooltip = if last_visible {
        format!("{name} — shown. At least one lane stays visible.")
    } else if row.visible {
        format!("{name} — shown. Click to hide.")
    } else {
        format!("{name} — hidden. Click to show.")
    };
    chip.button.set_tooltip_text(Some(&tooltip));
    chip.button
        .set_dropdown_tooltip(&format!("{name} lane options"));
    chip.button
        .update_property(&[gtk::accessible::Property::Label(&format!(
            "{name} lane, {}{}",
            if row.visible { "shown" } else { "hidden" },
            if paused { ", paused" } else { "" }
        ))]);

    let parameter = lane_parameter(chip.lane);
    let menu = gio::Menu::new();
    let show = gio::MenuItem::new(
        Some(if row.visible {
            "Hide lane"
        } else {
            "Show lane"
        }),
        None,
    );
    if !(last_visible && row.visible) {
        show.set_action_and_target_value(Some("lane.toggle"), Some(&parameter.to_variant()));
    }
    menu.append_item(&show);
    if let CaptionLane::Target(_) = chip.lane {
        if running {
            let (label, action) = if paused {
                ("Resume translation", "lane.resume")
            } else {
                ("Pause translation", "lane.pause")
            };
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), Some(&parameter.to_variant()));
            menu.append_item(&item);
        }
        let remove = gio::MenuItem::new(Some("Remove language"), None);
        if removable {
            remove.set_action_and_target_value(Some("lane.remove"), Some(&parameter.to_variant()));
        }
        menu.append_item(&remove);
    }
    chip.button.set_menu_model(Some(&menu));
}

#[cfg(test)]
mod tests {
    use lcrt_core::{CaptionLane, Language};

    use super::{LaneAction, lane_action, lane_parameter, parse_lane};

    #[test]
    fn lane_parameters_name_every_lane_unambiguously() {
        for lane in std::iter::once(CaptionLane::Source)
            .chain(Language::ALL.into_iter().map(CaptionLane::Target))
        {
            assert_eq!(parse_lane(&lane_parameter(lane)), Some(lane));
        }
        assert_eq!(parse_lane("xx"), None);
    }

    #[test]
    fn the_source_lane_can_only_be_shown_or_hidden() {
        let source = CaptionLane::Source;
        assert_eq!(
            lane_action("toggle", source),
            Some(LaneAction::Toggle(source))
        );
        for name in ["pause", "resume", "remove", "add"] {
            assert_eq!(lane_action(name, source), None);
        }
        let english = CaptionLane::Target(Language::English);
        assert_eq!(
            lane_action("pause", english),
            Some(LaneAction::Pause(Language::English))
        );
    }
}
