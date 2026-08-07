//! The channel strip's input selector: one button opening a popover that
//! combines the single-choice input kinds (nothing, virtual device, catch-all,
//! capture devices) with a checklist of applications, so one channel can
//! capture a whole group of apps.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::config::{self, Assignment};

use super::heading_label;

const CHECK_ICON: &str = "object-select-symbolic";

/// Slot for the handler invoked with a newly picked assignment.
type ChangeHandler = RefCell<Option<Rc<dyn Fn(Option<Assignment>)>>>;

/// Everything the picker renders: what the audio server currently offers plus
/// the bits of config that decide whether a row is available.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputOptions {
    /// This channel's name, for the "Crossfade: <name>" subtitle.
    pub channel: String,
    /// Capture devices as (description, source name).
    pub sources: Vec<(String, String)>,
    /// Monitors of output devices, same shape.
    pub monitors: Vec<(String, String)>,
    /// `application.name` of every playback stream currently running.
    pub apps: Vec<String>,
    /// Running apps another channel already claims, as (app, channel name).
    pub claimed: Vec<(String, String)>,
    /// Name of the channel that already is the catch-all, if it is not this
    /// one; that role can only be filled once.
    pub catch_all_owner: Option<String>,
}

struct Inner {
    label: gtk::Label,
    popover: gtk::Popover,
    inputs: gtk::ListBox,
    devices: gtk::ListBox,
    devices_group: gtk::Box,
    apps: gtk::ListBox,
    pattern: adw::EntryRow,
    changed: ChangeHandler,
    /// Latest state handed in by the UI, rendered from an idle callback so a
    /// row is never destroyed from inside its own signal handler.
    pending: RefCell<Option<(InputOptions, Option<Assignment>)>>,
    rendered: RefCell<Option<(InputOptions, Option<Assignment>)>>,
    render_queued: Cell<bool>,
}

/// One vertical input selector: a drop-down-styled button plus its popover.
pub struct InputPicker {
    pub root: gtk::MenuButton,
    inner: Rc<Inner>,
}

fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    list
}

/// Heading plus its boxed list, as one block in the popover.
fn section(title: &str, list: &gtk::ListBox) -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 6);
    group.append(&heading_label(title));
    group.append(list);
    group
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

impl InputPicker {
    pub fn new() -> Self {
        let label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(9)
            .width_chars(9)
            .label("No Input")
            .build();
        let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        button_box.append(&label);
        button_box.append(&gtk::Image::from_icon_name("pan-down-symbolic"));

        let popover = gtk::Popover::builder().build();

        let root = gtk::MenuButton::builder()
            .child(&button_box)
            .popover(&popover)
            .tooltip_text("Select what feeds this channel")
            .build();

        let inputs = boxed_list();
        let devices = boxed_list();
        let apps = boxed_list();
        let devices_group = section("Capture Devices", &devices);

        let pattern = adw::EntryRow::builder()
            .title("Add Pattern")
            .tooltip_text(
                "Match several applications at once, e.g. “Firefox*”. \
                 “*” stands for any text, “?” for a single character.",
            )
            .show_apply_button(true)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .width_request(300)
            .build();
        content.append(&section("Input", &inputs));
        content.append(&devices_group);
        content.append(&section("Applications", &apps));

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(460)
            .child(&content)
            .build();
        popover.set_child(Some(&scroller));

        let inner = Rc::new(Inner {
            label,
            popover,
            inputs,
            devices,
            devices_group,
            apps,
            pattern,
            changed: RefCell::new(None),
            pending: RefCell::new(None),
            rendered: RefCell::new(None),
            render_queued: Cell::new(false),
        });

        {
            let weak = Rc::downgrade(&inner);
            inner.pattern.connect_apply(move |entry| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let rule = entry.text().trim().to_string();
                entry.set_text("");
                if rule.is_empty() {
                    return;
                }
                let mut apps = inner.selected_apps();
                if apps.iter().any(|a| a.eq_ignore_ascii_case(&rule)) {
                    return;
                }
                apps.push(rule);
                inner.emit_apps(apps);
            });
        }

        Self { root, inner }
    }

    /// Register the handler invoked when the user picks a different input.
    pub fn connect_changed(&self, f: impl Fn(Option<Assignment>) + 'static) {
        *self.inner.changed.borrow_mut() = Some(Rc::new(f));
    }

    /// Show `current` against the devices and applications now available.
    /// Cheap to call on every device change: the rows are only rebuilt when
    /// something they show actually differs.
    pub fn update(&self, options: &InputOptions, current: &Option<Assignment>) {
        *self.inner.pending.borrow_mut() = Some((options.clone(), current.clone()));
        if self.inner.render_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(&self.inner);
        glib::idle_add_local_once(move || {
            if let Some(inner) = weak.upgrade() {
                inner.render_queued.set(false);
                inner.render();
            }
        });
    }
}

impl Inner {
    fn emit(&self, assignment: Option<Assignment>) {
        let changed = self.changed.borrow().clone();
        if let Some(changed) = changed {
            changed(assignment);
        }
    }

    /// Apply an edited rule list; without rules the channel is exactly a
    /// virtual device, so that is what it becomes.
    fn emit_apps(&self, apps: Vec<String>) {
        if apps.is_empty() {
            self.emit(Some(Assignment::Virtual));
        } else {
            self.emit(Some(Assignment::App { apps }));
        }
    }

    /// The rule list currently shown (empty unless this is an App channel).
    fn selected_apps(&self) -> Vec<String> {
        let apps = |state: &Option<(InputOptions, Option<Assignment>)>| {
            state
                .as_ref()
                .and_then(|(_, current)| current.as_ref())
                .map(|a| a.apps().to_vec())
        };
        apps(&self.pending.borrow())
            .or_else(|| apps(&self.rendered.borrow()))
            .unwrap_or_default()
    }

    /// A single-choice row; the selected one carries a check mark.
    fn choice_row(
        self: &Rc<Self>,
        title: &str,
        subtitle: &str,
        selected: bool,
        enabled: bool,
        target: Option<Assignment>,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(title))
            .activatable(true)
            .sensitive(enabled)
            .build();
        if !subtitle.is_empty() {
            row.set_subtitle(&glib::markup_escape_text(subtitle));
        }
        if selected {
            row.add_suffix(&gtk::Image::from_icon_name(CHECK_ICON));
        }
        let weak = Rc::downgrade(self);
        row.connect_activated(move |_| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            inner.popover.popdown();
            if !selected {
                inner.emit(target.clone());
            }
        });
        row
    }

    /// A checkable application row. `next` is the rule list the channel ends
    /// up with when the check is flipped.
    fn app_row(
        self: &Rc<Self>,
        title: &str,
        subtitle: &str,
        active: bool,
        enabled: bool,
        next: Vec<String>,
    ) -> adw::ActionRow {
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .active(active)
            .sensitive(enabled)
            .build();
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(title))
            .activatable_widget(&check)
            .sensitive(enabled)
            .build();
        if !subtitle.is_empty() {
            row.set_subtitle(&glib::markup_escape_text(subtitle));
        }
        row.add_prefix(&check);
        if enabled {
            let weak = Rc::downgrade(self);
            check.connect_toggled(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.emit_apps(next.clone());
                }
            });
        }
        row
    }

    fn placeholder(text: &str) -> adw::ActionRow {
        adw::ActionRow::builder()
            .title(glib::markup_escape_text(text))
            .sensitive(false)
            .build()
    }

    fn render(self: &Rc<Self>) {
        let Some((opts, current)) = self.pending.borrow_mut().take() else {
            return;
        };
        if *self.rendered.borrow() == Some((opts.clone(), current.clone())) {
            return;
        }
        let selected = current.as_ref().map(|a| a.apps().to_vec()).unwrap_or_default();
        let is_virtual = current
            .as_ref()
            .is_some_and(|a| matches!(a, Assignment::Virtual | Assignment::App { .. }));

        // ---- Input kinds --------------------------------------------------
        clear(&self.inputs);
        self.inputs
            .append(&self.choice_row("No Input", "", current.is_none(), true, None));
        self.inputs.append(&self.choice_row(
            "Virtual Device",
            &format!("Selectable as “Crossfade: {}”", opts.channel),
            is_virtual,
            true,
            Some(Assignment::Virtual),
        ));
        let catch_all = matches!(current, Some(Assignment::CatchAll));
        let catch_all_subtitle = match &opts.catch_all_owner {
            Some(owner) => format!("Already used by the “{owner}” channel"),
            None => "Every app no other channel captures".to_string(),
        };
        self.inputs.append(&self.choice_row(
            "Other Apps",
            &catch_all_subtitle,
            catch_all,
            catch_all || opts.catch_all_owner.is_none(),
            Some(Assignment::CatchAll),
        ));

        // ---- Capture devices ----------------------------------------------
        clear(&self.devices);
        let mut listed = false;
        let mut current_source_listed = false;
        for (description, name) in opts.sources.iter().chain(opts.monitors.iter()) {
            let target = Assignment::Source { name: name.clone() };
            let selected = current.as_ref() == Some(&target);
            current_source_listed |= selected;
            self.devices
                .append(&self.choice_row(description, "", selected, true, Some(target)));
            listed = true;
        }
        // Keep an unplugged device selectable so its assignment is not
        // silently lost while the hardware is away.
        if let Some(Assignment::Source { name }) = &current
            && !current_source_listed
        {
            let target = Assignment::Source { name: name.clone() };
            self.devices
                .append(&self.choice_row(name, "Not connected", true, true, Some(target)));
            listed = true;
        }
        self.devices_group.set_visible(listed);

        // ---- Applications --------------------------------------------------
        clear(&self.apps);
        for (i, rule) in selected.iter().enumerate() {
            let subtitle = if config::is_pattern(rule) {
                let matched: Vec<&str> = opts
                    .apps
                    .iter()
                    .filter(|a| config::app_matches(rule, a))
                    .map(String::as_str)
                    .collect();
                if matched.is_empty() {
                    "Pattern — nothing running matches".to_string()
                } else {
                    format!("Pattern — {}", matched.join(", "))
                }
            } else if opts.apps.iter().any(|a| config::app_matches(rule, a)) {
                String::new()
            } else {
                "Not running".to_string()
            };
            let mut next = selected.clone();
            next.remove(i);
            self.apps
                .append(&self.app_row(rule, &subtitle, true, true, next));
        }
        for app in &opts.apps {
            if selected
                .iter()
                .any(|r| !config::is_pattern(r) && config::app_matches(r, app))
            {
                continue;
            }
            let pattern = selected
                .iter()
                .find(|r| config::is_pattern(r) && config::app_matches(r, app));
            let owner = opts
                .claimed
                .iter()
                .find(|(a, _)| a == app)
                .map(|(_, channel)| channel.as_str());
            let row = if let Some(pattern) = pattern {
                self.app_row(app, &format!("Matched by “{pattern}”"), true, false, Vec::new())
            } else if let Some(owner) = owner {
                self.app_row(
                    app,
                    &format!("Captured by the “{owner}” channel"),
                    false,
                    false,
                    Vec::new(),
                )
            } else {
                let mut next = selected.clone();
                next.push(app.clone());
                self.app_row(app, "", false, true, next)
            };
            self.apps.append(&row);
        }
        if selected.is_empty() && opts.apps.is_empty() {
            self.apps
                .append(&Self::placeholder("No applications are playing audio"));
        }
        // Re-appended after every rebuild; the widget itself is kept so text
        // and focus survive a refresh while the popover is open.
        let focused = self.pattern.has_focus() || self.pattern.focus_child().is_some();
        if let Some(parent) = self.pattern.parent().and_downcast::<gtk::ListBox>() {
            parent.remove(&self.pattern);
        }
        self.apps.append(&self.pattern);
        if focused {
            self.pattern.grab_focus();
        }

        self.label.set_label(&Self::summary(&opts, &current));
        self.label.set_tooltip_text(Some(&Self::details(&opts, &current)));
        *self.rendered.borrow_mut() = Some((opts, current));
    }

    /// Short label for the button — the strip is only so wide.
    fn summary(opts: &InputOptions, current: &Option<Assignment>) -> String {
        match current {
            None => "No Input".to_string(),
            Some(Assignment::Virtual) => "Virtual Device".to_string(),
            Some(Assignment::CatchAll) => "Other Apps".to_string(),
            Some(Assignment::Source { name }) => Self::source_label(opts, name),
            Some(Assignment::App { apps }) => match apps.split_first() {
                Some((first, [])) => first.clone(),
                Some((first, rest)) => format!("{first} +{}", rest.len()),
                None => "Virtual Device".to_string(),
            },
        }
    }

    /// The button's tooltip: the whole truth, however long.
    fn details(opts: &InputOptions, current: &Option<Assignment>) -> String {
        match current {
            None => "No input".to_string(),
            Some(Assignment::Virtual) => {
                format!("Virtual device — “Crossfade: {}”", opts.channel)
            }
            Some(Assignment::CatchAll) => {
                "Every app no other channel captures".to_string()
            }
            Some(Assignment::Source { name }) => Self::source_label(opts, name),
            Some(Assignment::App { apps }) => apps.join(", "),
        }
    }

    fn source_label(opts: &InputOptions, name: &str) -> String {
        opts.sources
            .iter()
            .chain(opts.monitors.iter())
            .find(|(_, n)| n == name)
            .map(|(description, _)| description.clone())
            .unwrap_or_else(|| format!("{name} (unavailable)"))
    }
}
