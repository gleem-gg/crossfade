//! MIDI controller management dialog: connected controllers, behavior
//! options, and the binding profiles with their faders and pads.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;

use crate::config::{Config, MidiTarget};
use crate::midi::MidiManager;

pub struct MidiDeps {
    pub config: Rc<RefCell<Config>>,
    pub midi: MidiManager,
    /// Called after any change was written into the config.
    pub on_changed: Rc<dyn Fn()>,
    /// Opens the learn dialog for a target (used to bind profile pads).
    pub start_learn: Rc<dyn Fn(MidiTarget)>,
}

/// Human-readable description of a binding target, e.g. "Game — Monitor
/// volume". Also used by the learn dialog in `window.rs`.
pub fn target_description(cfg: &Config, target: &MidiTarget) -> String {
    let channel = |id: &u64| {
        cfg.channel(*id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "removed channel".to_string())
    };
    match target {
        MidiTarget::ChannelVolume { id, mix } => {
            format!("{} — {} volume", channel(id), mix.label())
        }
        MidiTarget::ChannelMute { id, mix } => format!("{} — {} mute", channel(id), mix.label()),
        MidiTarget::ChannelColor { id } => format!("{} — color pad", channel(id)),
        MidiTarget::MasterVolume { mix } => format!("{} Mix master volume", mix.label()),
        MidiTarget::MasterMute { mix } => format!("{} Mix master mute", mix.label()),
        MidiTarget::SelectProfile { profile } => format!(
            "switching to profile “{}”",
            cfg.midi
                .profile(*profile)
                .map(|p| p.name.as_str())
                .unwrap_or("?")
        ),
    }
}

type RefreshCell = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

fn rerun(refresh_cell: &RefreshCell) {
    let refresh = refresh_cell.borrow().clone();
    if let Some(refresh) = refresh {
        refresh();
    }
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let btn = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .build();
    btn.add_css_class("flat");
    btn.add_css_class("circular");
    btn
}

/// Opens the dialog; the returned hook rebuilds its content (run on device
/// hotplug and after bindings change from outside the dialog).
pub fn open(parent: &impl IsA<gtk::Widget>, deps: MidiDeps) -> (adw::Dialog, Rc<dyn Fn()>) {
    let deps = Rc::new(deps);
    let dialog = adw::Dialog::builder()
        .title("MIDI Controllers")
        .content_width(560)
        .content_height(640)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());

    if !deps.midi.available() {
        let page = adw::StatusPage::builder()
            .icon_name("audio-card-symbolic")
            .title("ALSA Sequencer Unavailable")
            .description(
                "MIDI controller support needs the ALSA sequencer \
                 (/dev/snd/seq), which could not be opened.",
            )
            .build();
        page.set_size_request(-1, 320);
        toolbar.set_content(Some(&page));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(parent));
        return (dialog, Rc::new(|| {}));
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(16);
    content.set_margin_end(16);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&content)
        .build();
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));

    // Profiles the user had expanded, preserved across rebuilds.
    let expanded: Rc<RefCell<HashSet<u64>>> = Rc::new(RefCell::new(HashSet::new()));
    let refresh_cell: RefreshCell = Rc::new(RefCell::new(None));
    let refresh: Rc<dyn Fn()> = {
        let deps = deps.clone();
        let content = content.clone();
        let expanded = expanded.clone();
        let refresh_cell = refresh_cell.clone();
        Rc::new(move || rebuild(&deps, &content, &expanded, &refresh_cell))
    };
    *refresh_cell.borrow_mut() = Some(refresh.clone());
    refresh();

    dialog.present(Some(parent));
    (dialog, refresh)
}

fn rebuild(
    deps: &Rc<MidiDeps>,
    content: &gtk::Box,
    expanded: &Rc<RefCell<HashSet<u64>>>,
    refresh_cell: &RefreshCell,
) {
    while let Some(child) = content.first_child() {
        content.remove(&child);
    }
    content.append(&devices_group(deps));
    content.append(&options_group(deps));
    content.append(&profiles_group(deps, expanded, refresh_cell));
}

fn devices_group(deps: &Rc<MidiDeps>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Controllers")
        .description(
            "Right-click any fader or mute button in the mixer to bind a \
             MIDI control to it (7-bit CC and note messages).",
        )
        .build();
    let devices = deps.midi.devices();
    if devices.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No Controllers Detected")
            .subtitle("Connect a MIDI controller — it is picked up automatically")
            .build();
        group.add(&row);
    } else {
        for name in devices {
            let row = adw::ActionRow::builder()
                .title(&name)
                .use_markup(false)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("audio-card-symbolic"));
            group.add(&row);
        }
    }
    group
}

fn options_group(deps: &Rc<MidiDeps>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Options").build();
    let (pickup_on, led_on, on_vel, off_vel) = {
        let cfg = deps.config.borrow();
        (
            cfg.midi.pickup,
            cfg.midi.led_feedback,
            cfg.midi.on_velocity,
            cfg.midi.off_velocity,
        )
    };

    let pickup = adw::SwitchRow::builder()
        .title("Fader Pickup")
        .subtitle(
            "A hardware fader takes effect once it crosses the current \
             level, so profile switches never jump the volume",
        )
        .active(pickup_on)
        .build();
    {
        let deps = deps.clone();
        pickup.connect_active_notify(move |row| {
            deps.config.borrow_mut().midi.pickup = row.is_active();
            (deps.on_changed)();
        });
    }
    group.add(&pickup);

    let led = adw::SwitchRow::builder()
        .title("LED Feedback")
        .subtitle("Light up pads bound to mutes (lit = muted) and to the active profile")
        .active(led_on)
        .build();
    {
        let deps = deps.clone();
        led.connect_active_notify(move |row| {
            deps.config.borrow_mut().midi.led_feedback = row.is_active();
            (deps.on_changed)();
        });
    }
    group.add(&led);

    let on_row = adw::SpinRow::with_range(0.0, 127.0, 1.0);
    on_row.set_title("Lit Pad Velocity");
    on_row.set_subtitle("Sent for a lit pad — selects the color on many pad controllers");
    on_row.set_value(on_vel as f64);
    {
        let deps = deps.clone();
        on_row.connect_value_notify(move |row| {
            deps.config.borrow_mut().midi.on_velocity = row.value() as u8;
            (deps.on_changed)();
        });
    }
    group.add(&on_row);

    let off_row = adw::SpinRow::with_range(0.0, 127.0, 1.0);
    off_row.set_title("Dark Pad Velocity");
    off_row.set_subtitle("Sent for a dark pad — 0 turns most pads off");
    off_row.set_value(off_vel as f64);
    {
        let deps = deps.clone();
        off_row.connect_value_notify(move |row| {
            deps.config.borrow_mut().midi.off_velocity = row.value() as u8;
            (deps.on_changed)();
        });
    }
    group.add(&off_row);

    group
}

fn profiles_group(
    deps: &Rc<MidiDeps>,
    expanded: &Rc<RefCell<HashSet<u64>>>,
    refresh_cell: &RefreshCell,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Profiles")
        .description(
            "Each profile is a bank of bindings; bind a pad to a profile \
             to switch banks from the controller",
        )
        .build();

    let add = gtk::Button::builder()
        .label("Add Profile")
        .valign(gtk::Align::Center)
        .build();
    add.add_css_class("flat");
    {
        let deps = deps.clone();
        let refresh_cell = refresh_cell.clone();
        add.connect_clicked(move |_| {
            deps.config.borrow_mut().midi.add_profile();
            (deps.on_changed)();
            rerun(&refresh_cell);
        });
    }
    group.set_header_suffix(Some(&add));

    let (profiles, active, globals) = {
        let cfg = deps.config.borrow();
        (
            cfg.midi.profiles.clone(),
            cfg.midi.active_profile,
            cfg.midi.global_bindings.clone(),
        )
    };
    let removable = profiles.len() > 1;
    let mut first_check: Option<gtk::CheckButton> = None;
    for profile in &profiles {
        let pid = profile.id;
        let pads: Vec<_> = globals
            .iter()
            .filter(|b| matches!(b.target, MidiTarget::SelectProfile { profile } if profile == pid))
            .cloned()
            .collect();
        let count = profile.bindings.len() + pads.len();
        let subtitle = match count {
            0 => "No bindings".to_string(),
            1 => "1 binding".to_string(),
            n => format!("{n} bindings"),
        };
        let row = adw::ExpanderRow::builder()
            .title(&profile.name)
            .subtitle(&subtitle)
            .use_markup(false)
            .build();

        // Radio: which profile is live.
        let check = gtk::CheckButton::new();
        check.set_valign(gtk::Align::Center);
        check.set_tooltip_text(Some("Use this profile"));
        if let Some(first) = &first_check {
            check.set_group(Some(first));
        } else {
            first_check = Some(check.clone());
        }
        check.set_active(pid == active);
        {
            let deps = deps.clone();
            let refresh_cell = refresh_cell.clone();
            check.connect_toggled(move |c| {
                if !c.is_active() {
                    return;
                }
                {
                    let mut cfg = deps.config.borrow_mut();
                    if cfg.midi.active_profile == pid {
                        return;
                    }
                    cfg.midi.active_profile = pid;
                }
                (deps.on_changed)();
                rerun(&refresh_cell);
            });
        }
        row.add_prefix(&check);

        let learn_btn = gtk::Button::builder()
            .label("Bind Pad…")
            .tooltip_text("Press a pad on the controller to bind switching to this profile")
            .valign(gtk::Align::Center)
            .build();
        learn_btn.add_css_class("flat");
        {
            let deps = deps.clone();
            learn_btn
                .connect_clicked(move |_| (deps.start_learn)(MidiTarget::SelectProfile {
                    profile: pid,
                }));
        }
        row.add_suffix(&learn_btn);

        let rename_btn = icon_button("document-edit-symbolic", "Rename profile");
        {
            let deps = deps.clone();
            let refresh_cell = refresh_cell.clone();
            let name = profile.name.clone();
            rename_btn.connect_clicked(move |btn| {
                open_rename_dialog(btn, &deps, &refresh_cell, pid, &name);
            });
        }
        row.add_suffix(&rename_btn);

        let delete_btn = icon_button("user-trash-symbolic", "Remove profile");
        delete_btn.set_sensitive(removable);
        if !removable {
            delete_btn.set_tooltip_text(Some("The last profile cannot be removed"));
        }
        {
            let deps = deps.clone();
            let refresh_cell = refresh_cell.clone();
            let name = profile.name.clone();
            delete_btn.connect_clicked(move |btn| {
                let confirm = adw::AlertDialog::builder()
                    .heading("Remove Profile?")
                    .body(format!(
                        "“{name}” will be removed, along with its bindings \
                         and any pads that switch to it."
                    ))
                    .default_response("cancel")
                    .close_response("cancel")
                    .build();
                confirm.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                confirm
                    .set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                let deps = deps.clone();
                let refresh_cell = refresh_cell.clone();
                confirm.connect_response(Some("remove"), move |_, _| {
                    deps.config.borrow_mut().midi.remove_profile(pid);
                    (deps.on_changed)();
                    rerun(&refresh_cell);
                });
                confirm.present(Some(btn));
            });
        }
        row.add_suffix(&delete_btn);

        row.set_expanded(expanded.borrow().contains(&pid));
        {
            let expanded = expanded.clone();
            row.connect_expanded_notify(move |r| {
                if r.is_expanded() {
                    expanded.borrow_mut().insert(pid);
                } else {
                    expanded.borrow_mut().remove(&pid);
                }
            });
        }

        {
            let cfg = deps.config.borrow();
            for binding in &profile.bindings {
                row.add_row(&binding_row(
                    deps,
                    refresh_cell,
                    pid,
                    &binding.source.label(),
                    &target_description(&cfg, &binding.target),
                    binding.target,
                ));
            }
        }
        for pad in &pads {
            row.add_row(&binding_row(
                deps,
                refresh_cell,
                pid,
                &pad.source.label(),
                "Switches to this profile",
                pad.target,
            ));
        }
        if count == 0 {
            let empty = adw::ActionRow::builder()
                .title("No Bindings")
                .subtitle("Right-click a fader or mute in the mixer to add one")
                .build();
            row.add_row(&empty);
        }

        group.add(&row);
    }
    group
}

fn binding_row(
    deps: &Rc<MidiDeps>,
    refresh_cell: &RefreshCell,
    profile_id: u64,
    title: &str,
    subtitle: &str,
    target: MidiTarget,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .use_markup(false)
        .build();
    let delete = icon_button("user-trash-symbolic", "Remove binding");
    {
        let deps = deps.clone();
        let refresh_cell = refresh_cell.clone();
        delete.connect_clicked(move |_| {
            deps.config.borrow_mut().midi.unbind(profile_id, &target);
            (deps.on_changed)();
            rerun(&refresh_cell);
        });
    }
    row.add_suffix(&delete);
    row
}

fn open_rename_dialog(
    parent: &impl IsA<gtk::Widget>,
    deps: &Rc<MidiDeps>,
    refresh_cell: &RefreshCell,
    profile_id: u64,
    current: &str,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Rename Profile")
        .default_response("rename")
        .close_response("cancel")
        .build();
    let entry = gtk::Entry::builder()
        .text(current)
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    let deps = deps.clone();
    let refresh_cell = refresh_cell.clone();
    dialog.connect_response(Some("rename"), move |_, _| {
        let name = entry.text().trim().to_string();
        if name.is_empty() {
            return;
        }
        if let Some(p) = deps
            .config
            .borrow_mut()
            .midi
            .profiles
            .iter_mut()
            .find(|p| p.id == profile_id)
        {
            p.name = name;
        }
        (deps.on_changed)();
        rerun(&refresh_cell);
    });
    dialog.present(Some(parent));
}
