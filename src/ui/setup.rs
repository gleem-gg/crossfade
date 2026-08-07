//! Audio setup assistant: checks that the system routes audio through
//! Crossfade (default output/input devices, and — when an Elgato Wave XLR is
//! connected — the microphone channel and monitor output), offering one-click
//! fixes. Shown automatically on first run; later launches only surface a
//! notice when something drifted.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::audio::{self, PulseManager, STREAM_MIC};
use crate::config::{Assignment, ChannelConfig, Config};

/// Everything the dialog needs from the main window.
pub struct SetupDeps {
    pub config: Rc<RefCell<Config>>,
    pub manager: PulseManager,
    /// Called after a fix changed the config: persist it and refresh the
    /// main window (device selectors, sidebar).
    pub on_changed: Rc<dyn Fn()>,
}

pub struct SetupItem {
    pub title: &'static str,
    pub subtitle: String,
    pub ok: bool,
    fix: SetupFix,
}

#[derive(Clone)]
enum SetupFix {
    /// Make the system play to the "Crossfade: System" virtual device.
    DefaultSink {
        channel_id: u64,
        sink: String,
        needs_assign: bool,
    },
    /// Make the system record from the "Crossfade Stream Mix" microphone.
    DefaultSource { source: String },
    /// Feed the Microphone channel from the Wave XLR capture device.
    MicAssignment { channel_id: u64, source: String },
    /// Play the monitor mix on the Wave XLR output.
    MonitorDevice { sink: String },
}

fn system_channel(config: &Config) -> Option<&ChannelConfig> {
    config
        .channels
        .iter()
        .find(|c| c.permanent && c.name == "System")
        .or_else(|| {
            config
                .channels
                .iter()
                .find(|c| c.assignment.as_ref().is_some_and(Assignment::provides_device))
        })
}

/// The channel standing in for "the microphone": the permanent one by that
/// name, else the first permanent channel that is not one of our own devices.
/// Shared with the tray indicator, which mutes exactly this channel.
pub fn mic_channel(config: &Config) -> Option<&ChannelConfig> {
    config
        .channels
        .iter()
        .find(|c| c.permanent && c.name == "Microphone")
        .or_else(|| {
            config.channels.iter().find(|c| {
                c.permanent
                    && !c.assignment.as_ref().is_some_and(Assignment::provides_device)
            })
        })
}

/// Evaluate every setup check against the current server + config state.
pub fn evaluate(config: &Config, manager: &PulseManager) -> Vec<SetupItem> {
    let mut items = Vec::new();

    if let Some(sys) = system_channel(config) {
        let sink = audio::channel_sink_name(sys.id);
        // Any kind that exposes the channel's own device will do — virtual,
        // app group or catch-all.
        let assigned = sys.assignment.as_ref().is_some_and(Assignment::provides_device);
        items.push(SetupItem {
            title: "Sound Output",
            subtitle: format!(
                "Use “Crossfade: {}” as the system’s output device",
                sys.name
            ),
            ok: assigned && manager.default_sink().as_deref() == Some(sink.as_str()),
            fix: SetupFix::DefaultSink {
                channel_id: sys.id,
                sink,
                needs_assign: !assigned,
            },
        });
    }

    items.push(SetupItem {
        title: "Sound Input",
        subtitle: "Use “Crossfade Stream Mix” as the system’s input device".to_string(),
        ok: manager.default_source().as_deref() == Some(STREAM_MIC),
        fix: SetupFix::DefaultSource {
            source: STREAM_MIC.to_string(),
        },
    });

    if let Some(src) = manager.wave_xlr_source()
        && let Some(mic) = mic_channel(config)
    {
        let want = Assignment::Source {
            name: src.name.clone(),
        };
        items.push(SetupItem {
            title: "Microphone Input",
            subtitle: format!("Feed the “{}” channel from “{}”", mic.name, src.description),
            ok: mic.assignment.as_ref() == Some(&want),
            fix: SetupFix::MicAssignment {
                channel_id: mic.id,
                source: src.name,
            },
        });
    }

    if let Some(sink) = manager.wave_xlr_sink() {
        items.push(SetupItem {
            title: "Monitor Output",
            subtitle: format!("Play the Monitor Mix on “{}”", sink.description),
            ok: config.master.monitor_device.as_deref() == Some(sink.name.as_str()),
            fix: SetupFix::MonitorDevice { sink: sink.name },
        });
    }

    items
}

/// True when every check passes (used for the startup notice).
pub fn all_ok(config: &Config, manager: &PulseManager) -> bool {
    evaluate(config, manager).iter().all(|i| i.ok)
}

fn apply_fix(deps: &SetupDeps, fix: &SetupFix) {
    match fix {
        SetupFix::DefaultSink {
            channel_id,
            sink,
            needs_assign,
        } => {
            if *needs_assign {
                if let Some(ch) = deps.config.borrow_mut().channel_mut(*channel_id) {
                    ch.assignment = Some(Assignment::Virtual);
                }
                deps.manager.rebuild_channel(*channel_id);
                (deps.on_changed)();
            }
            set_default_when_ready(deps.manager.clone(), sink.clone(), true);
        }
        SetupFix::DefaultSource { source } => {
            set_default_when_ready(deps.manager.clone(), source.clone(), false);
        }
        SetupFix::MicAssignment { channel_id, source } => {
            if let Some(ch) = deps.config.borrow_mut().channel_mut(*channel_id) {
                ch.assignment = Some(Assignment::Source {
                    name: source.clone(),
                });
            }
            deps.manager.rebuild_channel(*channel_id);
            (deps.on_changed)();
        }
        SetupFix::MonitorDevice { sink } => {
            deps.config.borrow_mut().master.monitor_device = Some(sink.clone());
            deps.manager.setup_monitor_output();
            (deps.on_changed)();
        }
    }
}

/// Make a device the default as soon as it exists on the server. A freshly
/// (re)created channel sink appears asynchronously, so poll briefly instead
/// of failing when the fix raced the module load.
fn set_default_when_ready(manager: PulseManager, name: String, is_sink: bool) {
    let mut tries = 0;
    glib::timeout_add_local(Duration::from_millis(250), move || {
        let exists = if is_sink {
            manager.has_sink(&name)
        } else {
            manager.has_source(&name)
        };
        if exists {
            if is_sink {
                manager.set_default_sink(&name);
            } else {
                manager.set_default_source(&name);
            }
            return glib::ControlFlow::Break;
        }
        tries += 1;
        if tries >= 20 {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

struct DialogState {
    deps: SetupDeps,
    list: gtk::ListBox,
    fix_all: gtk::Button,
    all_ok_label: gtk::Label,
}

/// Present the dialog; returns it together with a refresh hook the window
/// calls whenever devices change, so the status icons update live.
pub fn open(parent: &impl IsA<gtk::Widget>, deps: SetupDeps) -> (adw::Dialog, Rc<dyn Fn()>) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(16);
    content.set_margin_end(16);

    let intro = gtk::Label::builder()
        .label(
            "Crossfade works best when the system plays all audio through it \
             and streaming apps record the stream mix. The checks below turn \
             green once everything is in place.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    intro.add_css_class("dim-label");
    content.append(&intro);

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    content.append(&list);

    let fix_all = gtk::Button::builder()
        .label("Apply Recommended Settings")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    content.append(&fix_all);

    let all_ok_label = gtk::Label::builder()
        .label("Everything is set up correctly.")
        .visible(false)
        .build();
    all_ok_label.add_css_class("dim-label");
    content.append(&all_ok_label);

    let state = Rc::new(DialogState {
        deps,
        list,
        fix_all: fix_all.clone(),
        all_ok_label,
    });
    rebuild(&state);

    {
        let state = state.clone();
        fix_all.connect_clicked(move |_| {
            let items = {
                let cfg = state.deps.config.borrow();
                evaluate(&cfg, &state.deps.manager)
            };
            for item in items.iter().filter(|i| !i.ok) {
                apply_fix(&state.deps, &item.fix);
            }
            schedule_rebuild(&state);
        });
    }

    let dialog = adw::Dialog::builder()
        .title("Audio Setup")
        .content_width(480)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));

    let refresh: Rc<dyn Fn()> = {
        let state = state.clone();
        Rc::new(move || rebuild(&state))
    };
    (dialog, refresh)
}

/// Fixes take effect asynchronously on the server; rebuild shortly after so
/// rows the fix resolved immediately flip without waiting for a device event.
fn schedule_rebuild(state: &Rc<DialogState>) {
    let state = state.clone();
    glib::timeout_add_local_once(Duration::from_millis(300), move || rebuild(&state));
}

fn rebuild(state: &Rc<DialogState>) {
    while let Some(child) = state.list.first_child() {
        state.list.remove(&child);
    }
    let items = {
        let cfg = state.deps.config.borrow();
        evaluate(&cfg, &state.deps.manager)
    };
    let any_bad = items.iter().any(|i| !i.ok);
    for item in items {
        let row = adw::ActionRow::builder()
            .title(item.title)
            .subtitle(&item.subtitle)
            .use_markup(false)
            .build();

        let icon = gtk::Image::from_icon_name(if item.ok {
            "object-select-symbolic"
        } else {
            "dialog-warning-symbolic"
        });
        icon.add_css_class(if item.ok { "success" } else { "warning" });
        icon.set_tooltip_text(Some(if item.ok {
            "Configured correctly"
        } else {
            "Not configured yet"
        }));
        row.add_prefix(&icon);

        if !item.ok {
            let fix = gtk::Button::builder()
                .label("Fix")
                .valign(gtk::Align::Center)
                .build();
            let state = state.clone();
            let action = item.fix.clone();
            fix.connect_clicked(move |_| {
                apply_fix(&state.deps, &action);
                schedule_rebuild(&state);
            });
            row.add_suffix(&fix);
        }
        state.list.append(&row);
    }
    state.fix_all.set_visible(any_bad);
    state.all_ok_label.set_visible(!any_bad);
}
