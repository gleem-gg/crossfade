use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::audio::{AudioEvent, LevelTarget, Mix, PulseManager};
use crate::config::{
    Assignment, Config, MAX_CHANNELS, MidiBinding, MidiKind, MidiSource, MidiTarget,
};
use crate::midi::{MidiEvent, MidiManager};

use super::channel_strip::ChannelStrip;
use super::dbus;
use super::effects::{self, EffectsDeps};
use super::heading_label;
use super::midi as midi_ui;
use super::outputs::OutputsPanel;
use super::setup;
use super::sidebar::Sidebar;
use super::wave_xlr;

/// An armed "Learn MIDI" dialog waiting for the next hardware event.
struct LearnRequest {
    target: MidiTarget,
    /// Profile the binding is stored in (the active one when armed).
    profile: u64,
    /// Closes the dialog and reports what was captured.
    done: Box<dyn Fn(MidiSource)>,
}

/// Runtime fader-pickup / edge-detection state per bound hardware control.
/// Never persisted; cleared on profile switches and binding edits.
struct PickupState {
    engaged: bool,
    /// Previous incoming value (0..1); -1 before the first event.
    last_in: f64,
    /// Target value as of our last write, to detect outside changes.
    last_written: f64,
}

struct App {
    config: Rc<RefCell<Config>>,
    manager: PulseManager,
    midi: MidiManager,
    window: glib::WeakRef<adw::ApplicationWindow>,
    toasts: adw::ToastOverlay,
    /// Channel strips currently shown, each paired with its channel id.
    strips: RefCell<Vec<(u64, ChannelStrip)>>,
    strips_box: gtk::Box,
    add_button: gtk::MenuButton,
    add_menu: gio::Menu,
    outputs: OutputsPanel,
    sidebar: Sidebar,
    stack: gtk::Stack,
    error_page: adw::StatusPage,
    save_pending: Cell<bool>,
    /// Per-channel edit counter used to debounce rename → sink rebuild.
    rename_epoch: RefCell<HashMap<u64, u64>>,
    /// The open effects dialog (channel id + hooks), so VST rack load
    /// results and native-UI parameter edits can update it live.
    fx_dialog: RefCell<Option<(u64, Rc<effects::DialogHooks>)>>,
    /// Refresh hook of the open setup dialog, driven by device changes.
    setup_hook: RefCell<Option<Rc<dyn Fn()>>>,
    /// First-run dialog / misconfiguration notice already handled this
    /// session.
    setup_prompted: Cell<bool>,
    /// Wave XLR volumes were restored for the current device appearance;
    /// reset when the device disappears so a replug restores them again.
    /// Enforcement deadlines for the stored Wave XLR startup volumes; None
    /// while the device is absent, set on each appearance.
    xlr_mic_hold: Cell<Option<Instant>>,
    xlr_out_hold: Cell<Option<Instant>>,
    /// Forces every strip and the add-channel card to the same width.
    strip_size_group: gtk::SizeGroup,
    midi_learn: RefCell<Option<LearnRequest>>,
    midi_pickup: RefCell<HashMap<MidiSource, PickupState>>,
    /// Last velocity sent per (device, MIDI channel, note), so LED feedback
    /// only transmits actual changes.
    midi_led: RefCell<HashMap<(String, u8, u8), u8>>,
    /// Refresh hook of the open MIDI controllers dialog.
    midi_hook: RefCell<Option<Rc<dyn Fn()>>>,
    /// Emits the D-Bus StateChanged signal (None when off the session bus).
    dbus_signal: RefCell<Option<Rc<dyn Fn()>>>,
}

pub fn build(application: &adw::Application) -> adw::ApplicationWindow {
    let config = Rc::new(RefCell::new(Config::load()));
    let manager = PulseManager::new(config.clone());
    let outputs = OutputsPanel::new();
    let sidebar = Sidebar::new();

    outputs.load_config(&config.borrow().master);
    outputs.set_vod_visible(config.borrow().vod_mix_enabled);

    // ---- Mixer page ----------------------------------------------------------
    // Strips keep a fixed width; the row scrolls instead of stretching.
    let strips_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    strips_box.set_halign(gtk::Align::Start);
    let strips_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .child(&strips_box)
        .vexpand(true)
        .build();

    let add_menu = gio::Menu::new();
    let add_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    add_content.set_valign(gtk::Align::Center);
    let add_icon = gtk::Image::from_icon_name("list-add-symbolic");
    add_icon.set_pixel_size(24);
    add_content.append(&add_icon);
    let add_label = gtk::Label::new(Some("Add Channel"));
    add_label.add_css_class("dim-label");
    add_content.append(&add_label);
    let add_button = gtk::MenuButton::builder()
        .child(&add_content)
        .menu_model(&add_menu)
        .tooltip_text("Add a channel")
        .width_request(super::channel_strip::STRIP_WIDTH)
        .build();
    add_button.add_css_class("card");
    add_button.add_css_class("flat");

    let strip_size_group = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
    strip_size_group.add_widget(&add_button);

    let mixer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    mixer.append(&heading_label("Inputs"));
    mixer.append(&strips_scroller);
    mixer.append(&heading_label("Outputs"));
    mixer.append(&outputs.root);

    // Threshold = maximum: use the full window width up to the cap; without
    // this the clamp eases side margins in long before the strips fit.
    let clamp = adw::Clamp::builder()
        .maximum_size(1600)
        .tightening_threshold(1600)
        .child(&mixer)
        .build();

    // ---- Status pages ----------------------------------------------------------
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    let connecting_page = adw::StatusPage::builder()
        .icon_name("audio-card-symbolic")
        .title("Connecting to Audio Server")
        .description("Creating the virtual mix devices…")
        .child(&spinner)
        .build();

    let retry = gtk::Button::builder()
        .label("Retry")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    let error_page = adw::StatusPage::builder()
        .icon_name("audio-volume-muted-symbolic")
        .title("Audio Server Unavailable")
        .child(&retry)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&connecting_page, Some("connecting"));
    stack.add_named(&error_page, Some("error"));
    stack.add_named(&clamp, Some("mixer"));

    // ---- Window chrome ----------------------------------------------------------
    let header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new("Gleem Crossfade", "Dual-Mix Audio Router"))
        .build();

    let menu = gio::Menu::new();
    let menu_settings = gio::Menu::new();
    menu_settings.append(Some("Audio Setup…"), Some("win.setup"));
    menu_settings.append(Some("Wave XLR…"), Some("win.wave-xlr"));
    menu_settings.append(Some("MIDI Controllers…"), Some("win.midi"));
    menu_settings.append(Some("Enable VOD Mix"), Some("win.vod-mix"));
    menu_settings.append(Some("Start at Login"), Some("win.autostart"));
    menu.append_section(None, &menu_settings);
    let menu_general = gio::Menu::new();
    menu_general.append(Some("About"), Some("win.about"));
    menu_general.append(Some("Quit"), Some("app.quit"));
    menu.append_section(None, &menu_general);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main Menu")
        .build();
    header.pack_end(&menu_button);

    let sidebar_toggle = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-right-symbolic")
        .tooltip_text("Show Devices")
        .build();
    header.pack_end(&sidebar_toggle);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));

    let split = adw::OverlaySplitView::builder()
        .content(&toolbar)
        .sidebar(&sidebar.root)
        .sidebar_position(gtk::PackType::End)
        .show_sidebar(false)
        .max_sidebar_width(320.0)
        .build();
    sidebar_toggle
        .bind_property("active", &split, "show-sidebar")
        .bidirectional()
        .sync_create()
        .build();

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Gleem Crossfade")
        .default_width(1280)
        .default_height(720)
        .content(&split)
        .build();

    let app = Rc::new(App {
        config,
        manager,
        midi: MidiManager::new(),
        window: window.downgrade(),
        toasts,
        strips: RefCell::new(Vec::new()),
        strips_box,
        add_button,
        add_menu,
        outputs,
        sidebar,
        stack,
        error_page,
        save_pending: Cell::new(false),
        rename_epoch: RefCell::new(HashMap::new()),
        fx_dialog: RefCell::new(None),
        setup_hook: RefCell::new(None),
        setup_prompted: Cell::new(false),
        xlr_mic_hold: Cell::new(None),
        xlr_out_hold: Cell::new(None),
        strip_size_group,
        midi_learn: RefCell::new(None),
        midi_pickup: RefCell::new(HashMap::new()),
        midi_led: RefCell::new(HashMap::new()),
        midi_hook: RefCell::new(None),
        dbus_signal: RefCell::new(None),
    });

    wire_actions(&app, &window);
    wire_outputs(&app);
    wire_audio_events(&app);
    wire_midi_events(&app);
    app.midi.start();
    {
        let perform_action: Rc<dyn Fn(ControlAction)> = {
            let app = app.clone();
            Rc::new(move |action| perform(&app, action))
        };
        *app.dbus_signal.borrow_mut() = dbus::register(
            application,
            dbus::DbusDeps {
                config: app.config.clone(),
                perform: perform_action,
            },
        );
    }
    {
        let app = app.clone();
        retry.connect_clicked(move |_| {
            app.stack.set_visible_child_name("connecting");
            app.manager.connect_server();
        });
    }
    wire_close(&app, &window);
    wire_quit(&app, application, &window);

    // A window started with --hidden gets its setup check when it is first
    // shown instead of at startup.
    {
        let app = app.clone();
        window.connect_map(move |_| {
            let app = app.clone();
            glib::timeout_add_local_once(Duration::from_secs(1), move || {
                maybe_prompt_setup(&app);
            });
        });
    }

    rebuild_strips(&app);
    app.manager.connect_server();
    window
}

// ---- Persistence -----------------------------------------------------------

fn schedule_save(app: &Rc<App>) {
    if app.save_pending.replace(true) {
        return;
    }
    let app = app.clone();
    glib::timeout_add_local_once(Duration::from_millis(700), move || {
        app.save_pending.set(false);
        app.config.borrow().save();
        // Every state mutation funnels through here, so this doubles as the
        // (coalesced) D-Bus change notification.
        let signal = app.dbus_signal.borrow().clone();
        if let Some(signal) = signal {
            signal();
        }
    });
}

// ---- Reactions to audio-server events ---------------------------------------

fn wire_audio_events(app: &Rc<App>) {
    let weak = Rc::downgrade(app);
    app.manager.set_event_handler(move |ev| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        match ev {
            AudioEvent::Ready => {
                app.stack.set_visible_child_name("mixer");
                refresh_leds(&app);
                // Give the buses/loopbacks a moment to settle before judging
                // the setup, so startup churn doesn't read as misconfigured.
                let app = app.clone();
                glib::timeout_add_local_once(Duration::from_secs(3), move || {
                    maybe_prompt_setup(&app);
                });
            }
            AudioEvent::Failed(msg) => {
                app.error_page.set_description(Some(&msg));
                app.stack.set_visible_child_name("error");
            }
            AudioEvent::DevicesChanged => {
                refresh_devices(&app);
                restore_wave_xlr(&app);
                let hook = app.setup_hook.borrow().clone();
                if let Some(refresh) = hook {
                    refresh();
                }
            }
            AudioEvent::Level(target, v) => match target {
                LevelTarget::Channel(id) => {
                    if let Some((_, strip)) =
                        app.strips.borrow().iter().find(|(cid, _)| *cid == id)
                    {
                        strip.set_levels(&v);
                    }
                }
                LevelTarget::MonitorMix => app.outputs.monitor_level.set_levels(&v),
                LevelTarget::StreamMix => app.outputs.stream_level.set_levels(&v),
                LevelTarget::VodMix => app.outputs.vod_level.set_levels(&v),
            },
            AudioEvent::VstChanged(id) => {
                let hooks = app
                    .fx_dialog
                    .borrow()
                    .as_ref()
                    .filter(|(did, _)| *did == id)
                    .map(|(_, h)| h.clone());
                if let Some(hooks) = hooks {
                    (hooks.refresh)();
                }
            }
            AudioEvent::VstParams(id, updates) => {
                schedule_save(&app);
                let hooks = app
                    .fx_dialog
                    .borrow()
                    .as_ref()
                    .filter(|(did, _)| *did == id)
                    .map(|(_, h)| h.clone());
                if let Some(hooks) = hooks {
                    (hooks.sync_params)(&updates);
                }
            }
        }
    });
}

// ---- Control core -----------------------------------------------------------

/// One mixer mutation, shared by MIDI dispatch and the D-Bus API (and any
/// future remote-control surface). Actions drive the same widgets the user
/// would, so the existing signal handlers do the config write, server
/// apply, link-follow and debounced save — one code path for everything.
#[derive(Clone, Copy, Debug)]
pub enum ControlAction {
    SetChannelVolume { id: u64, mix: Mix, value: f64 },
    /// `muted: None` toggles.
    SetChannelMute { id: u64, mix: Mix, muted: Option<bool> },
    SetMasterVolume { mix: Mix, value: f64 },
    SetMasterMute { mix: Mix, muted: Option<bool> },
    SelectMidiProfile { id: u64 },
}

/// The per-mix widgets of a channel strip, cloned out so no `strips` borrow
/// is held while a `set_value`/`set_active` runs its handlers.
fn channel_widgets(app: &App, id: u64, mix: Mix) -> Option<(gtk::Scale, gtk::ToggleButton)> {
    let strips = app.strips.borrow();
    let (_, strip) = strips.iter().find(|(cid, _)| *cid == id)?;
    Some(match mix {
        Mix::Monitor => (strip.monitor_scale.clone(), strip.monitor_mute.clone()),
        Mix::Stream => (strip.stream_scale.clone(), strip.stream_mute.clone()),
        Mix::Vod => (strip.vod_scale.clone(), strip.vod_mute.clone()),
    })
}

fn perform(app: &Rc<App>, action: ControlAction) {
    // VOD targets are inert while the VOD mix is disabled.
    let vod_off = |mix: Mix| mix == Mix::Vod && !app.config.borrow().vod_mix_enabled;
    match action {
        ControlAction::SetChannelVolume { id, mix, value } => {
            if vod_off(mix) {
                return;
            }
            let Some((scale, _)) = channel_widgets(app, id, mix) else {
                return;
            };
            scale.set_value(value.clamp(0.0, 1.0));
        }
        ControlAction::SetChannelMute { id, mix, muted } => {
            if vod_off(mix) {
                return;
            }
            let Some((_, mute)) = channel_widgets(app, id, mix) else {
                return;
            };
            mute.set_active(muted.unwrap_or(!mute.is_active()));
        }
        ControlAction::SetMasterVolume { mix, value } => {
            if vod_off(mix) {
                return;
            }
            let scale = match mix {
                Mix::Monitor => app.outputs.monitor_scale.clone(),
                Mix::Stream => app.outputs.stream_scale.clone(),
                Mix::Vod => app.outputs.vod_scale.clone(),
            };
            scale.set_value(value.clamp(0.0, 1.0));
        }
        ControlAction::SetMasterMute { mix, muted } => {
            if vod_off(mix) {
                return;
            }
            let mute = match mix {
                Mix::Monitor => app.outputs.monitor_mute.clone(),
                Mix::Stream => app.outputs.stream_mute.clone(),
                Mix::Vod => app.outputs.vod_mute.clone(),
            };
            mute.set_active(muted.unwrap_or(!mute.is_active()));
        }
        ControlAction::SelectMidiProfile { id } => {
            {
                let mut cfg = app.config.borrow_mut();
                if cfg.midi.profile(id).is_none() || cfg.midi.active_profile == id {
                    return;
                }
                cfg.midi.active_profile = id;
            }
            // Faders bound in the new profile must pick up their targets.
            app.midi_pickup.borrow_mut().clear();
            schedule_save(app);
            refresh_leds(app);
            let hook = app.midi_hook.borrow().clone();
            if let Some(hook) = hook {
                hook();
            }
            let name = app
                .config
                .borrow()
                .midi
                .profile(id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            app.toasts
                .add_toast(adw::Toast::new(&format!("MIDI profile: {name}")));
        }
    }
}

// ---- MIDI dispatch ----------------------------------------------------------

fn wire_midi_events(app: &Rc<App>) {
    let weak = Rc::downgrade(app);
    app.midi.set_event_handler(move |ev| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        match ev {
            MidiEvent::DevicesChanged => {
                // Forget what a replugged controller was showing so its
                // pads are fully re-sent, then update LEDs and the dialog.
                let devices = app.midi.devices();
                app.midi_led
                    .borrow_mut()
                    .retain(|key, _| devices.contains(&key.0));
                refresh_leds(&app);
                let hook = app.midi_hook.borrow().clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            MidiEvent::Control {
                device,
                channel,
                number,
                value,
            } => handle_midi_input(
                &app,
                MidiSource {
                    device,
                    channel,
                    kind: MidiKind::Cc,
                    number,
                },
                value,
            ),
            MidiEvent::NoteOn {
                device,
                channel,
                number,
                ..
            } => handle_midi_input(
                &app,
                MidiSource {
                    device,
                    channel,
                    kind: MidiKind::Note,
                    number,
                },
                127,
            ),
        }
    });
}

fn handle_midi_input(app: &Rc<App>, source: MidiSource, value: u8) {
    if app.midi_learn.borrow().is_some() {
        learn_capture(app, source);
        return;
    }
    let target = {
        let cfg = app.config.borrow();
        cfg.midi
            .global_bindings
            .iter()
            .chain(cfg.midi.active().bindings.iter())
            .find(|b| b.source == source)
            .map(|b| b.target)
    };
    let Some(target) = target else {
        return;
    };
    match target {
        MidiTarget::ChannelVolume { id, mix } => {
            if let Some(v) = fader_value(app, &source, value, &target) {
                perform(app, ControlAction::SetChannelVolume { id, mix, value: v });
            }
        }
        MidiTarget::MasterVolume { mix } => {
            if let Some(v) = fader_value(app, &source, value, &target) {
                perform(app, ControlAction::SetMasterVolume { mix, value: v });
            }
        }
        MidiTarget::ChannelMute { id, mix } => {
            if pressed(app, &source, value) {
                perform(app, ControlAction::SetChannelMute { id, mix, muted: None });
            }
        }
        MidiTarget::MasterMute { mix } => {
            if pressed(app, &source, value) {
                perform(app, ControlAction::SetMasterMute { mix, muted: None });
            }
        }
        MidiTarget::SelectProfile { profile } => {
            if pressed(app, &source, value) {
                perform(app, ControlAction::SelectMidiProfile { id: profile });
            }
        }
        // Color pads are pure locator LEDs; pressing one does nothing.
        MidiTarget::ChannelColor { .. } => {}
    }
}

/// Map a CC event onto a volume target, applying fader pickup. Returns the
/// value to apply, or None while the event should be swallowed.
fn fader_value(app: &Rc<App>, source: &MidiSource, value: u8, target: &MidiTarget) -> Option<f64> {
    if source.kind != MidiKind::Cc {
        return None;
    }
    let v = value as f64 / 127.0;
    let cur = {
        let cfg = app.config.borrow();
        match *target {
            MidiTarget::ChannelVolume { id, mix } => {
                let c = cfg.channel(id)?;
                match mix {
                    Mix::Monitor => c.monitor_volume,
                    Mix::Stream => c.stream_volume,
                    Mix::Vod => c.vod_volume,
                }
            }
            MidiTarget::MasterVolume { mix } => match mix {
                Mix::Monitor => cfg.master.monitor_volume,
                Mix::Stream => cfg.master.stream_volume,
                Mix::Vod => cfg.master.vod_volume,
            },
            _ => return None,
        }
    };
    pickup_allows(app, source, v, cur).then_some(v)
}

/// Fader pickup: a hardware fader that is out of sync with its target
/// (profile switch, GUI drag, link-follow) must cross the current value
/// once before it takes over, so it never jumps the volume to wherever the
/// fader happens to sit.
fn pickup_allows(app: &Rc<App>, source: &MidiSource, v: f64, cur: f64) -> bool {
    if !app.config.borrow().midi.pickup {
        return true;
    }
    let mut map = app.midi_pickup.borrow_mut();
    let st = map.entry(source.clone()).or_insert(PickupState {
        engaged: false,
        last_in: -1.0,
        last_written: f64::NAN,
    });
    // Something else moved the target since our last write: back to pickup
    // until the fader catches the new value.
    if st.engaged && (cur - st.last_written).abs() > 0.02 {
        st.engaged = false;
    }
    if !st.engaged
        && ((v - cur).abs() <= 0.03
            || (st.last_in >= 0.0 && (st.last_in - cur) * (v - cur) <= 0.0))
    {
        st.engaged = true;
    }
    st.last_in = v;
    if st.engaged {
        st.last_written = v;
        true
    } else {
        false
    }
}

/// Whether this event counts as a button press. Notes always do (note-offs
/// never reach this layer); CC buttons act once per rising edge through the
/// midpoint, so momentary pads sending 127/0 toggle cleanly and a swept
/// knob toggles only once.
fn pressed(app: &Rc<App>, source: &MidiSource, value: u8) -> bool {
    if source.kind == MidiKind::Note {
        return true;
    }
    let mut map = app.midi_pickup.borrow_mut();
    let st = map.entry(source.clone()).or_insert(PickupState {
        engaged: false,
        last_in: -1.0,
        last_written: f64::NAN,
    });
    let v = value as f64 / 127.0;
    let was_low = st.last_in < 0.5;
    st.last_in = v;
    v >= 0.5 && was_low
}

fn learn_capture(app: &Rc<App>, source: MidiSource) {
    {
        // Volume targets need a continuous control; ignore pad presses
        // while one is being learned instead of mis-binding them.
        let learn = app.midi_learn.borrow();
        let Some(req) = learn.as_ref() else {
            return;
        };
        let volume = matches!(
            req.target,
            MidiTarget::ChannelVolume { .. } | MidiTarget::MasterVolume { .. }
        );
        if volume && source.kind == MidiKind::Note {
            return;
        }
        // Color pads are pure LEDs and only note pads can be lit, so ignore
        // CC input while one is being learned.
        if matches!(req.target, MidiTarget::ChannelColor { .. }) && source.kind != MidiKind::Note {
            return;
        }
    }
    let Some(req) = app.midi_learn.borrow_mut().take() else {
        return;
    };
    app.config.borrow_mut().midi.bind(
        req.profile,
        MidiBinding {
            source: source.clone(),
            target: req.target,
        },
    );
    app.midi_pickup.borrow_mut().clear();
    schedule_save(app);
    refresh_leds(app);
    let hook = app.midi_hook.borrow().clone();
    if let Some(hook) = hook {
        hook();
    }
    (req.done)(source);
}

// ---- MIDI LED feedback ------------------------------------------------------

/// Push mute / active-profile state to note-bound pads, transmitting only
/// actual changes. Pads whose binding disappeared (or all of them, when LED
/// feedback is off) are blanked.
fn refresh_leds(app: &Rc<App>) {
    let (desired, off) = {
        let cfg = app.config.borrow();
        let mut desired: HashMap<(String, u8, u8), u8> = HashMap::new();
        if cfg.midi.led_feedback {
            let (on, off) = (cfg.midi.on_velocity, cfg.midi.off_velocity);
            for b in cfg
                .midi
                .global_bindings
                .iter()
                .chain(cfg.midi.active().bindings.iter())
            {
                if b.source.kind != MidiKind::Note {
                    continue;
                }
                let vel = match b.target {
                    MidiTarget::ChannelMute { id, mix } => {
                        if mix == Mix::Vod && !cfg.vod_mix_enabled {
                            continue;
                        }
                        let Some(c) = cfg.channel(id) else {
                            continue;
                        };
                        let lit = match mix {
                            Mix::Monitor => c.monitor_muted,
                            Mix::Stream => c.stream_muted,
                            Mix::Vod => c.vod_muted,
                        };
                        if lit { on } else { off }
                    }
                    MidiTarget::MasterMute { mix } => {
                        if mix == Mix::Vod && !cfg.vod_mix_enabled {
                            continue;
                        }
                        let lit = match mix {
                            Mix::Monitor => cfg.master.monitor_muted,
                            Mix::Stream => cfg.master.stream_muted,
                            Mix::Vod => cfg.master.vod_muted,
                        };
                        if lit { on } else { off }
                    }
                    MidiTarget::SelectProfile { profile } => {
                        if profile == cfg.midi.active_profile { on } else { off }
                    }
                    // Color pads glow in the channel's color (nearest entry
                    // of the pad palette); dark while the channel has none.
                    MidiTarget::ChannelColor { id } => {
                        let Some(c) = cfg.channel(id) else {
                            continue;
                        };
                        match c.color_rgb() {
                            Some((r, g, b)) => crate::midi::color_velocity(r, g, b),
                            None => off,
                        }
                    }
                    _ => continue,
                };
                desired.insert(
                    (b.source.device.clone(), b.source.channel, b.source.number),
                    vel,
                );
            }
        }
        (desired, cfg.midi.off_velocity)
    };
    let mut cache = app.midi_led.borrow_mut();
    let stale: Vec<(String, u8, u8)> = cache
        .keys()
        .filter(|k| !desired.contains_key(*k))
        .cloned()
        .collect();
    for key in stale {
        app.midi.send_note(&key.0, key.1, key.2, off);
        cache.remove(&key);
    }
    for (key, vel) in desired {
        if cache.get(&key) == Some(&vel) {
            continue;
        }
        app.midi.send_note(&key.0, key.1, key.2, vel);
        cache.insert(key, vel);
    }
}

// ---- MIDI learn UI ----------------------------------------------------------

/// Right-clicking a fader or mute opens the MIDI learn dialog for it.
fn attach_learn(app: &Rc<App>, widget: &impl IsA<gtk::Widget>, target: MidiTarget) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
    let app = app.clone();
    gesture.connect_pressed(move |_, _, _, _| open_learn_dialog(&app, target));
    widget.add_controller(gesture);
}

fn open_learn_dialog(app: &Rc<App>, target: MidiTarget) {
    let Some(window) = app.window.upgrade() else {
        return;
    };
    if !app.midi.available() {
        app.toasts.add_toast(adw::Toast::new(
            "MIDI is unavailable — the ALSA sequencer could not be opened",
        ));
        return;
    }
    let (profile, desc, bound) = {
        let cfg = app.config.borrow();
        let bindings = if matches!(target, MidiTarget::SelectProfile { .. }) {
            &cfg.midi.global_bindings
        } else {
            &cfg.midi.active().bindings
        };
        (
            cfg.midi.active_profile,
            midi_ui::target_description(&cfg, &target),
            bindings.iter().any(|b| b.target == target),
        )
    };
    let dialog = adw::AlertDialog::builder()
        .heading("Learn MIDI Control")
        .body(format!(
            "Move a fader or press a pad on your MIDI controller to bind it to {desc}."
        ))
        .default_response("cancel")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", "Cancel")]);
    if bound {
        dialog.add_responses(&[("clear", "Remove Binding")]);
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
    }
    {
        let toasts = app.toasts.clone();
        let dlg = dialog.clone();
        *app.midi_learn.borrow_mut() = Some(LearnRequest {
            target,
            profile,
            done: Box::new(move |source| {
                toasts.add_toast(adw::Toast::new(&format!("Bound to {}", source.label())));
                dlg.close();
            }),
        });
    }
    {
        let app = app.clone();
        dialog.connect_response(Some("clear"), move |_, _| {
            app.config.borrow_mut().midi.unbind(profile, &target);
            schedule_save(&app);
            refresh_leds(&app);
            let hook = app.midi_hook.borrow().clone();
            if let Some(hook) = hook {
                hook();
            }
        });
    }
    {
        let app = app.clone();
        dialog.connect_closed(move |_| {
            *app.midi_learn.borrow_mut() = None;
        });
    }
    dialog.present(Some(&window));
}

fn open_midi_dialog(app: &Rc<App>) {
    if app.midi_hook.borrow().is_some() {
        return;
    }
    let Some(window) = app.window.upgrade() else {
        return;
    };
    let on_changed: Rc<dyn Fn()> = {
        let app = app.clone();
        Rc::new(move || {
            app.midi_pickup.borrow_mut().clear();
            schedule_save(&app);
            refresh_leds(&app);
        })
    };
    let start_learn: Rc<dyn Fn(MidiTarget)> = {
        let app = app.clone();
        Rc::new(move |target| open_learn_dialog(&app, target))
    };
    let (dialog, refresh) = midi_ui::open(
        &window,
        midi_ui::MidiDeps {
            config: app.config.clone(),
            midi: app.midi.clone(),
            on_changed,
            start_learn,
        },
    );
    *app.midi_hook.borrow_mut() = Some(refresh);
    let app = app.clone();
    dialog.connect_closed(move |_| {
        *app.midi_hook.borrow_mut() = None;
    });
}

fn refresh_devices(app: &Rc<App>) {
    let sources = app.manager.sources();
    let apps = app.manager.app_names();

    let mut items: Vec<(String, Option<Assignment>)> = vec![
        ("No Input".to_string(), None),
        ("Virtual Device".to_string(), Some(Assignment::Virtual)),
    ];
    for s in sources.iter().filter(|s| !s.is_monitor) {
        items.push((
            s.description.clone(),
            Some(Assignment::Source {
                name: s.name.clone(),
            }),
        ));
    }
    for a in &apps {
        items.push((
            format!("{a} — Application"),
            Some(Assignment::App { name: a.clone() }),
        ));
    }
    for s in sources.iter().filter(|s| s.is_monitor) {
        items.push((
            s.description.clone(),
            Some(Assignment::Source {
                name: s.name.clone(),
            }),
        ));
    }

    let sinks = app.manager.output_sinks();
    let mut sink_items: Vec<(String, Option<String>)> =
        vec![("System Default".to_string(), None)];
    for s in &sinks {
        sink_items.push((s.description.clone(), Some(s.name.clone())));
    }

    {
        let cfg = app.config.borrow();
        for (id, strip) in app.strips.borrow().iter() {
            if let Some(ch) = cfg.channel(*id) {
                strip.set_input_entries(&items, &ch.assignment);
            }
        }
        app.outputs
            .set_output_sinks(&sink_items, &cfg.master.monitor_device);
    }
    update_sidebar(app);
}

fn update_sidebar(app: &Rc<App>) {
    let cfg = app.config.borrow().clone();
    let sinks = app.manager.output_sinks();
    let monitor_label = cfg
        .master
        .monitor_device
        .as_ref()
        .and_then(|d| {
            sinks
                .iter()
                .find(|s| &s.name == d)
                .map(|s| s.description.clone())
        })
        .unwrap_or_else(|| "system default output".to_string());
    let manager = app.manager.clone();
    app.sidebar.update(&cfg, &monitor_label, &move |a| match a {
        Assignment::Source { name } => manager
            .source_description(name)
            .unwrap_or_else(|| name.clone()),
        Assignment::App { name } => format!("{name} — application"),
        Assignment::Virtual => "Virtual device".to_string(),
    });
}

// ---- Channel strips (dynamic) -------------------------------------------------

/// Recreate all channel strips from the config. Called on startup and after
/// adding/removing a channel.
fn rebuild_strips(app: &Rc<App>) {
    while let Some(child) = app.strips_box.first_child() {
        app.strips_box.remove(&child);
    }
    let channels = app.config.borrow().channels.clone();
    let vod = app.config.borrow().vod_mix_enabled;
    let mut strips = Vec::with_capacity(channels.len());
    for ch in &channels {
        let strip = ChannelStrip::new();
        strip.load_config(ch);
        strip.set_vod_visible(vod);
        if ch.permanent {
            // Shown but disabled: permanent strips keep the exact same
            // header layout as removable ones.
            strip.remove.set_sensitive(false);
            strip.remove.set_tooltip_text(Some("Built-in channels cannot be removed"));
        }
        wire_strip(app, &strip, ch.id);
        app.strips_box.append(&strip.root);
        app.strip_size_group.add_widget(&strip.root);
        strips.push((ch.id, strip));
    }
    app.strips_box.append(&app.add_button);
    app.add_button.set_visible(channels.len() < MAX_CHANNELS);
    *app.strips.borrow_mut() = strips;
    rebuild_add_menu(app);
    refresh_devices(app);
}

fn rebuild_add_menu(app: &Rc<App>) {
    app.add_menu.remove_all();
    let templates = app.config.borrow().unused_template_names();
    for name in templates {
        app.add_menu
            .append(Some(name), Some(&format!("win.add-channel('{name}')")));
    }
    app.add_menu
        .append(Some("Custom Channel"), Some("win.add-channel('')"));
}

fn wire_strip(app: &Rc<App>, strip: &ChannelStrip, id: u64) {
    // Right-click on any fader or mute binds a MIDI control to it; on the
    // color button it binds a pad that lights up in the channel's color.
    attach_learn(app, &strip.color, MidiTarget::ChannelColor { id });
    attach_learn(app, &strip.monitor_scale, MidiTarget::ChannelVolume { id, mix: Mix::Monitor });
    attach_learn(app, &strip.stream_scale, MidiTarget::ChannelVolume { id, mix: Mix::Stream });
    attach_learn(app, &strip.vod_scale, MidiTarget::ChannelVolume { id, mix: Mix::Vod });
    attach_learn(app, &strip.monitor_mute, MidiTarget::ChannelMute { id, mix: Mix::Monitor });
    attach_learn(app, &strip.stream_mute, MidiTarget::ChannelMute { id, mix: Mix::Stream });
    attach_learn(app, &strip.vod_mute, MidiTarget::ChannelMute { id, mix: Mix::Vod });

    {
        let app = app.clone();
        strip.connect_color_changed(move |hex| {
            {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.color = hex;
            }
            schedule_save(&app);
            refresh_leds(&app);
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        strip.name.connect_changed(move |editable| {
            if guard.get() {
                return;
            }
            let text = editable.text().to_string();
            {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.name = text;
            }
            schedule_save(&app);
            update_sidebar(&app);
            rebuild_add_menu(&app);
            // Virtual/App channels expose a device named after the channel;
            // rebuild the channel sink once the user stops typing.
            let epoch = {
                let mut epochs = app.rename_epoch.borrow_mut();
                let e = epochs.entry(id).or_insert(0);
                *e += 1;
                *e
            };
            let app = app.clone();
            glib::timeout_add_local_once(Duration::from_millis(900), move || {
                if app.rename_epoch.borrow().get(&id) != Some(&epoch) {
                    return;
                }
                let needs_sink = matches!(
                    app.config.borrow().channel(id).and_then(|c| c.assignment.clone()),
                    Some(Assignment::App { .. }) | Some(Assignment::Virtual)
                );
                if needs_sink {
                    app.manager.rebuild_channel(id);
                }
            });
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        let entries = strip.entries.clone();
        strip.input.connect_selected_notify(move |dd| {
            if guard.get() {
                return;
            }
            let assignment = entries
                .borrow()
                .get(dd.selected() as usize)
                .cloned()
                .flatten();
            {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                if ch.assignment == assignment {
                    return;
                }
                ch.assignment = assignment;
            }
            app.manager.rebuild_channel(id);
            schedule_save(&app);
            update_sidebar(&app);
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        let others = [strip.stream_scale.clone(), strip.vod_scale.clone()];
        strip.monitor_scale.connect_value_changed(move |scale| {
            if guard.get() {
                return;
            }
            let v = scale.value();
            let linked = {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.monitor_volume = v;
                ch.linked
            };
            app.manager.apply_channel_mix(id, Mix::Monitor);
            if linked {
                for other in &others {
                    other.set_value(v);
                }
            }
            schedule_save(&app);
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        let others = [strip.monitor_scale.clone(), strip.vod_scale.clone()];
        strip.stream_scale.connect_value_changed(move |scale| {
            if guard.get() {
                return;
            }
            let v = scale.value();
            let linked = {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.stream_volume = v;
                ch.linked
            };
            app.manager.apply_channel_mix(id, Mix::Stream);
            if linked {
                for other in &others {
                    other.set_value(v);
                }
            }
            schedule_save(&app);
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        let others = [strip.monitor_scale.clone(), strip.stream_scale.clone()];
        strip.vod_scale.connect_value_changed(move |scale| {
            if guard.get() {
                return;
            }
            let v = scale.value();
            let linked = {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.vod_volume = v;
                ch.linked
            };
            app.manager.apply_channel_mix(id, Mix::Vod);
            if linked {
                for other in &others {
                    other.set_value(v);
                }
            }
            schedule_save(&app);
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        strip.monitor_mute.connect_toggled(move |btn| {
            if guard.get() {
                return;
            }
            {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.monitor_muted = btn.is_active();
            }
            app.manager.apply_channel_mix(id, Mix::Monitor);
            schedule_save(&app);
            refresh_leds(&app);
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        strip.stream_mute.connect_toggled(move |btn| {
            if guard.get() {
                return;
            }
            {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.stream_muted = btn.is_active();
            }
            app.manager.apply_channel_mix(id, Mix::Stream);
            schedule_save(&app);
            refresh_leds(&app);
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        strip.vod_mute.connect_toggled(move |btn| {
            if guard.get() {
                return;
            }
            {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.vod_muted = btn.is_active();
            }
            app.manager.apply_channel_mix(id, Mix::Vod);
            schedule_save(&app);
            refresh_leds(&app);
        });
    }

    {
        let app = app.clone();
        let guard = strip.guard.clone();
        let monitor_scale = strip.monitor_scale.clone();
        let stream_scale = strip.stream_scale.clone();
        let vod_scale = strip.vod_scale.clone();
        strip.link.connect_toggled(move |btn| {
            if guard.get() {
                return;
            }
            {
                let mut cfg = app.config.borrow_mut();
                let Some(ch) = cfg.channel_mut(id) else {
                    return;
                };
                ch.linked = btn.is_active();
            }
            if btn.is_active() {
                stream_scale.set_value(monitor_scale.value());
                vod_scale.set_value(monitor_scale.value());
            }
            schedule_save(&app);
        });
    }

    {
        let app = app.clone();
        strip.fx.connect_clicked(move |btn| {
            let on_structure = {
                let app = app.clone();
                Rc::new(move |id: u64| {
                    app.manager.rebuild_channel(id);
                    schedule_save(&app);
                    update_sidebar(&app);
                    let cfg = app.config.borrow();
                    if let Some(ch) = cfg.channel(id)
                        && let Some((_, strip)) =
                            app.strips.borrow().iter().find(|(cid, _)| *cid == id)
                        {
                            strip.update_fx_indicator(ch);
                        }
                })
            };
            let on_control = {
                let app = app.clone();
                Rc::new(move |_id: u64| schedule_save(&app))
            };
            let (dialog, hooks) = effects::open(
                btn,
                EffectsDeps {
                    config: app.config.clone(),
                    manager: app.manager.clone(),
                    on_structure,
                    on_control,
                },
                id,
            );
            *app.fx_dialog.borrow_mut() = Some((id, Rc::new(hooks)));
            let app = app.clone();
            dialog.connect_closed(move |_| {
                let mut open = app.fx_dialog.borrow_mut();
                if open.as_ref().is_some_and(|(did, _)| *did == id) {
                    *open = None;
                }
            });
        });
    }

    {
        let app = app.clone();
        strip.remove.connect_clicked(move |btn| {
            let name = app
                .config
                .borrow()
                .channel(id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let confirm = adw::AlertDialog::builder()
                .heading("Remove Channel?")
                .body(format!(
                    "“{name}” will be removed, along with its input assignment, \
                     mix levels and effects."
                ))
                .default_response("cancel")
                .close_response("cancel")
                .build();
            confirm.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
            confirm.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            let app = app.clone();
            confirm.connect_response(Some("remove"), move |_, _| {
                {
                    let mut cfg = app.config.borrow_mut();
                    cfg.remove_channel(id);
                    cfg.midi.remove_channel_bindings(id);
                }
                app.manager.rebuild_channel(id);
                rebuild_strips(&app);
                schedule_save(&app);
                update_sidebar(&app);
                refresh_leds(&app);
            });
            confirm.present(Some(btn));
        });
    }
}

// ---- Outputs section -----------------------------------------------------------

fn wire_outputs(app: &Rc<App>) {
    attach_learn(app, &app.outputs.monitor_scale, MidiTarget::MasterVolume { mix: Mix::Monitor });
    attach_learn(app, &app.outputs.stream_scale, MidiTarget::MasterVolume { mix: Mix::Stream });
    attach_learn(app, &app.outputs.vod_scale, MidiTarget::MasterVolume { mix: Mix::Vod });
    attach_learn(app, &app.outputs.monitor_mute, MidiTarget::MasterMute { mix: Mix::Monitor });
    attach_learn(app, &app.outputs.stream_mute, MidiTarget::MasterMute { mix: Mix::Stream });
    attach_learn(app, &app.outputs.vod_mute, MidiTarget::MasterMute { mix: Mix::Vod });

    {
        let app = app.clone();
        app.clone()
            .outputs
            .monitor_device
            .connect_selected_notify(move |dd| {
                if app.outputs.guard.get() {
                    return;
                }
                let device = app
                    .outputs
                    .sink_entries
                    .borrow()
                    .get(dd.selected() as usize)
                    .cloned()
                    .flatten();
                {
                    let mut cfg = app.config.borrow_mut();
                    if cfg.master.monitor_device == device {
                        return;
                    }
                    cfg.master.monitor_device = device;
                }
                app.manager.setup_monitor_output();
                schedule_save(&app);
                update_sidebar(&app);
            });
    }
    {
        let app = app.clone();
        app.clone()
            .outputs
            .monitor_scale
            .connect_value_changed(move |scale| {
                if app.outputs.guard.get() {
                    return;
                }
                app.config.borrow_mut().master.monitor_volume = scale.value();
                app.manager.apply_master_monitor();
                schedule_save(&app);
            });
    }
    {
        let app = app.clone();
        app.clone()
            .outputs
            .monitor_mute
            .connect_toggled(move |btn| {
                if app.outputs.guard.get() {
                    return;
                }
                app.config.borrow_mut().master.monitor_muted = btn.is_active();
                app.manager.apply_master_monitor();
                schedule_save(&app);
                refresh_leds(&app);
            });
    }
    {
        let app = app.clone();
        app.clone()
            .outputs
            .stream_scale
            .connect_value_changed(move |scale| {
                if app.outputs.guard.get() {
                    return;
                }
                app.config.borrow_mut().master.stream_volume = scale.value();
                app.manager.apply_master_stream();
                schedule_save(&app);
            });
    }
    {
        let app = app.clone();
        app.clone().outputs.stream_mute.connect_toggled(move |btn| {
            if app.outputs.guard.get() {
                return;
            }
            app.config.borrow_mut().master.stream_muted = btn.is_active();
            app.manager.apply_master_stream();
            schedule_save(&app);
            refresh_leds(&app);
        });
    }
    {
        let app = app.clone();
        app.clone()
            .outputs
            .vod_scale
            .connect_value_changed(move |scale| {
                if app.outputs.guard.get() {
                    return;
                }
                app.config.borrow_mut().master.vod_volume = scale.value();
                app.manager.apply_master_vod();
                schedule_save(&app);
            });
    }
    {
        let app = app.clone();
        app.clone().outputs.vod_mute.connect_toggled(move |btn| {
            if app.outputs.guard.get() {
                return;
            }
            app.config.borrow_mut().master.vod_muted = btn.is_active();
            app.manager.apply_master_vod();
            schedule_save(&app);
            refresh_leds(&app);
        });
    }
}

// ---- Setup assistant --------------------------------------------------------

/// First run: present the setup assistant. Later runs: if the system audio
/// drifted away from the recommended setup, show a notice with a shortcut to
/// the assistant. Runs at most once per session, and only while the window
/// is actually visible.
fn maybe_prompt_setup(app: &Rc<App>) {
    if app.setup_prompted.get() {
        return;
    }
    let Some(window) = app.window.upgrade() else {
        return;
    };
    if !window.is_visible()
        || app.stack.visible_child_name().as_deref() != Some("mixer")
    {
        return;
    }
    let (first_run, all_ok) = {
        let cfg = app.config.borrow();
        (!cfg.setup_done, setup::all_ok(&cfg, &app.manager))
    };
    app.setup_prompted.set(true);
    if first_run {
        app.config.borrow_mut().setup_done = true;
        schedule_save(app);
        open_setup(app);
    } else if !all_ok {
        let toast = adw::Toast::builder()
            .title("The system audio setup needs attention")
            .button_label("Review")
            .timeout(0)
            .build();
        {
            let app = app.clone();
            toast.connect_button_clicked(move |_| open_setup(&app));
        }
        app.toasts.add_toast(toast);
    }
}

fn open_setup(app: &Rc<App>) {
    if app.setup_hook.borrow().is_some() {
        return;
    }
    let Some(window) = app.window.upgrade() else {
        return;
    };
    let on_changed: Rc<dyn Fn()> = {
        let app = app.clone();
        Rc::new(move || {
            schedule_save(&app);
            refresh_devices(&app);
            update_sidebar(&app);
        })
    };
    let (dialog, refresh) = setup::open(
        &window,
        setup::SetupDeps {
            config: app.config.clone(),
            manager: app.manager.clone(),
            on_changed,
        },
    );
    *app.setup_hook.borrow_mut() = Some(refresh);
    let app = app.clone();
    dialog.connect_closed(move |_| {
        *app.setup_hook.borrow_mut() = None;
    });
}

// ---- Wave XLR ---------------------------------------------------------------

fn open_wave_xlr(app: &Rc<App>) {
    let Some(window) = app.window.upgrade() else {
        return;
    };
    let on_changed: Rc<dyn Fn()> = {
        let app = app.clone();
        Rc::new(move || schedule_save(&app))
    };
    wave_xlr::open(
        &window,
        wave_xlr::XlrDeps {
            config: app.config.clone(),
            manager: app.manager.clone(),
            on_changed,
        },
    );
}

/// How long the stored Wave XLR startup volumes are enforced after startup
/// or a device (re)appearance. A single write is not enough: the device's
/// node suspends/resumes while the channels wire up, WirePlumber re-applies
/// its own stored route volumes on activation, and the firmware itself
/// occasionally resets to 100% — whichever write lands last wins, so during
/// this window every refresh that shows a drifted volume re-applies ours
/// (each reset raises a change event, so no polling is needed). Afterwards
/// the device's physical controls are left alone.
const XLR_ENFORCE_WINDOW: Duration = Duration::from_secs(15);

fn restore_wave_xlr(app: &Rc<App>) {
    let (mic, out) = {
        let cfg = app.config.borrow();
        (cfg.wave_xlr.mic_volume, cfg.wave_xlr.output_volume)
    };
    let mic_dev = app.manager.wave_xlr_source().map(|s| (s.name, s.volume));
    let out_dev = app.manager.wave_xlr_sink().map(|s| (s.name, s.volume));
    enforce_xlr_volume(mic_dev, mic, &app.xlr_mic_hold, |name, pct| {
        app.manager.set_source_volume(name, pct);
    });
    enforce_xlr_volume(out_dev, out, &app.xlr_out_hold, |name, pct| {
        app.manager.set_sink_volume(name, pct);
    });
}

fn enforce_xlr_volume(
    dev: Option<(String, f64)>,
    stored: Option<f64>,
    hold: &Cell<Option<Instant>>,
    apply: impl Fn(&str, f64),
) {
    let Some((name, current)) = dev else {
        hold.set(None);
        return;
    };
    let deadline = match hold.get() {
        Some(d) => d,
        None => {
            let d = Instant::now() + XLR_ENFORCE_WINDOW;
            hold.set(Some(d));
            d
        }
    };
    if Instant::now() > deadline {
        return;
    }
    if let Some(pct) = stored
        && (current - pct).abs() > 1.0
    {
        apply(&name, pct);
    }
}

// ---- Window actions -------------------------------------------------------------

// ---- Autostart -------------------------------------------------------------

fn autostart_file() -> std::path::PathBuf {
    glib::user_config_dir()
        .join("autostart")
        .join(format!("{}.desktop", crate::APP_ID))
}

fn autostart_enabled() -> bool {
    autostart_file().exists()
}

/// Enable/disable launch-on-login via an XDG autostart entry. The entry runs
/// the current executable with --hidden so the virtual devices come up in
/// the background without opening the window.
pub(crate) fn set_autostart(enable: bool) -> std::io::Result<()> {
    let path = autostart_file();
    if !enable {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "gleem-crossfade".to_string());
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Gleem Crossfade\n\
             Comment=Dual-mix virtual audio mixer for streaming\n\
             Exec={exe} --hidden\n\
             Icon={}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            crate::APP_ID
        ),
    )
}

fn wire_actions(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let autostart = gio::SimpleAction::new_stateful(
        "autostart",
        None,
        &autostart_enabled().to_variant(),
    );
    autostart.connect_activate(|action, _| {
        let enable = !action
            .state()
            .and_then(|s| s.get::<bool>())
            .unwrap_or(false);
        match set_autostart(enable) {
            Ok(()) => action.set_state(&enable.to_variant()),
            Err(e) => eprintln!("gleem-crossfade: could not update autostart entry: {e}"),
        }
    });
    window.add_action(&autostart);

    let add = gio::SimpleAction::new("add-channel", Some(glib::VariantTy::STRING));
    {
        let app = app.clone();
        add.connect_activate(move |_, param| {
            let name = param.and_then(|v| v.str()).unwrap_or("");
            let name = if name.is_empty() { None } else { Some(name) };
            let id = app.config.borrow_mut().add_channel(name);
            if let Some(id) = id {
                app.manager.rebuild_channel(id);
                rebuild_strips(&app);
                schedule_save(&app);
                update_sidebar(&app);
            }
        });
    }
    window.add_action(&add);

    let vod = gio::SimpleAction::new_stateful(
        "vod-mix",
        None,
        &app.config.borrow().vod_mix_enabled.to_variant(),
    );
    {
        let app = app.clone();
        vod.connect_activate(move |action, _| {
            let enable = !action
                .state()
                .and_then(|s| s.get::<bool>())
                .unwrap_or(false);
            action.set_state(&enable.to_variant());
            app.config.borrow_mut().vod_mix_enabled = enable;
            app.manager.apply_vod_mix();
            app.outputs.set_vod_visible(enable);
            for (_, strip) in app.strips.borrow().iter() {
                strip.set_vod_visible(enable);
            }
            schedule_save(&app);
            update_sidebar(&app);
            refresh_leds(&app);
        });
    }
    window.add_action(&vod);

    let midi_action = gio::SimpleAction::new("midi", None);
    {
        let app = app.clone();
        midi_action.connect_activate(move |_, _| open_midi_dialog(&app));
    }
    window.add_action(&midi_action);

    let setup_action = gio::SimpleAction::new("setup", None);
    {
        let app = app.clone();
        setup_action.connect_activate(move |_, _| open_setup(&app));
    }
    window.add_action(&setup_action);

    let xlr_action = gio::SimpleAction::new("wave-xlr", None);
    {
        let app = app.clone();
        xlr_action.connect_activate(move |_, _| open_wave_xlr(&app));
    }
    window.add_action(&xlr_action);

    let about = gio::SimpleAction::new("about", None);
    let win_weak = window.downgrade();
    about.connect_activate(move |_, _| {
        if let Some(win) = win_weak.upgrade() {
            let dialog = adw::AboutDialog::builder()
                .application_name("Gleem Crossfade")
                .application_icon("gg.gleem.Crossfade")
                .developer_name("René Preuß")
                .copyright("© 2026 René Preuß")
                .version(env!("CARGO_PKG_VERSION"))
                .comments(
                    "Dual-mix virtual audio mixer for Linux. \
                     Route hardware inputs and applications into independent \
                     monitor and stream mixes.",
                )
                .license_type(gtk::License::MitX11)
                .build();
            dialog.present(Some(&win));
        }
    });
    window.add_action(&about);
}

/// Closing the window only hides it: the virtual devices and all routing
/// keep working in the background. Launching the app again (or activating
/// it from the shell) brings the window back; "Quit" tears everything down.
fn wire_close(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let app = app.clone();
    let notified = Cell::new(false);
    window.connect_close_request(move |win| {
        app.config.borrow().save();
        win.set_visible(false);
        if !notified.replace(true)
            && let Some(gapp) = win.application() {
                let note = gio::Notification::new("Gleem Crossfade is still running");
                note.set_body(Some(
                    "The virtual audio devices stay active in the background. \
                     Use Quit in the main menu to stop them.",
                ));
                gapp.send_notification(Some("gleem-crossfade-background"), &note);
            }
        glib::Propagation::Stop
    });
}

/// app.quit: save, unload everything we created on the audio server, then
/// really exit.
fn wire_quit(app: &Rc<App>, application: &adw::Application, window: &adw::ApplicationWindow) {
    let quit = gio::SimpleAction::new("quit", None);
    let outer_application = application.clone();
    let app = app.clone();
    let application = application.clone();
    let window = window.clone();
    quit.connect_activate(move |_, _| {
        app.config.borrow().save();
        // Leave controller pads dark instead of frozen at the last state.
        let lit: Vec<(String, u8, u8)> = app.midi_led.borrow_mut().drain().map(|(k, _)| k).collect();
        let off = app.config.borrow().midi.off_velocity;
        for (device, channel, note) in lit {
            app.midi.send_note(&device, channel, note, off);
        }
        let done = Rc::new(Cell::new(false));
        let finish = {
            let application = application.clone();
            let window = window.clone();
            move || {
                window.destroy();
                application.quit();
            }
        };
        {
            let done = done.clone();
            let finish = finish.clone();
            app.manager.shutdown(Box::new(move || {
                if !done.replace(true) {
                    finish();
                }
            }));
        }
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if !done.replace(true) {
                finish();
            }
        });
    });
    outer_application.add_action(&quit);
}
