//! Per-channel effects dialog: an ordered LV2 plugin chain with inline
//! parameter editing, plus the optional Carla VST rack.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::audio::AudioManager;
use crate::config::Config;
use crate::fx::{VstParamInfo, VstState};
use crate::lv2;
use crate::vst;

/// Everything the dialog needs from the main window.
pub struct EffectsDeps {
    pub config: Rc<RefCell<Config>>,
    pub manager: AudioManager,
    /// Called after add/remove/reorder/enable/rack changes: persist config,
    /// rebuild the channel routing and refresh the main UI.
    pub on_structure: Rc<dyn Fn(u64)>,
    /// Called after a live control-value change: persist config only.
    pub on_control: Rc<dyn Fn(u64)>,
}

enum VstParamWidget {
    Scale(gtk::Scale),
    Switch(gtk::Switch),
}

struct DialogState {
    channel_id: u64,
    deps: EffectsDeps,
    /// Container the LV2 group is rebuilt into.
    effects_box: gtk::Box,
    dialog: adw::Dialog,
    /// Trailing-edge debouncer for pw-cli control updates while a slider
    /// is being dragged.
    control_debounce: RefCell<Option<glib::SourceId>>,
    /// VST parameter widgets by (plugin cfg_id, param index), so edits made
    /// in a plugin's native window can move the sliders here live.
    vst_widgets: RefCell<HashMap<(u64, u32), VstParamWidget>>,
    /// Set while widgets are updated programmatically; their change
    /// handlers must not write back (the value came from the engine).
    syncing: Cell<bool>,
}

/// Parameter updates from a plugin's native window:
/// [(plugin cfg_id, param index, value)].
pub type ParamSync = Rc<dyn Fn(&[(u64, u32, f64)])>;

/// Hooks the window keeps for the open dialog.
pub struct DialogHooks {
    /// Rebuild everything (a rack finished loading).
    pub refresh: Rc<dyn Fn()>,
    /// Move parameter widgets to values edited in a plugin's native window.
    pub sync_params: ParamSync,
}

/// Present the dialog; returns it together with the update hooks.
pub fn open(
    parent: &impl IsA<gtk::Widget>,
    deps: EffectsDeps,
    channel_id: u64,
) -> (adw::Dialog, DialogHooks) {
    let channel_name = deps
        .config
        .borrow()
        .channel(channel_id)
        .map(|c| c.name.clone())
        .unwrap_or_default();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(16);
    content.set_margin_end(16);

    let effects_box = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.append(&effects_box);

    let dialog = adw::Dialog::builder()
        .title(format!("Effects — {channel_name}"))
        .content_width(560)
        .content_height(640)
        .build();

    let state = Rc::new(DialogState {
        channel_id,
        deps,
        effects_box,
        dialog: dialog.clone(),
        control_debounce: RefCell::new(None),
        vst_widgets: RefCell::new(HashMap::new()),
        syncing: Cell::new(false),
    });

    rebuild_groups(&state);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&adw::Clamp::builder().maximum_size(520).child(&content).build())
        .vexpand(true)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));

    let refresh: Rc<dyn Fn()> = {
        let state = state.clone();
        Rc::new(move || rebuild_groups(&state))
    };
    let sync_params: ParamSync = {
        let state = state.clone();
        Rc::new(move |updates| {
            state.syncing.set(true);
            let widgets = state.vst_widgets.borrow();
            for (cfg_id, index, value) in updates {
                match widgets.get(&(*cfg_id, *index)) {
                    Some(VstParamWidget::Scale(s)) => s.set_value(*value),
                    Some(VstParamWidget::Switch(sw)) => sw.set_active(*value > 0.5),
                    None => {}
                }
            }
            state.syncing.set(false);
        })
    };
    (dialog, DialogHooks { refresh, sync_params })
}

/// (Re)build both groups from config + runtime state.
fn rebuild_groups(state: &Rc<DialogState>) {
    state.vst_widgets.borrow_mut().clear();
    while let Some(child) = state.effects_box.first_child() {
        state.effects_box.remove(&child);
    }
    state.effects_box.append(&build_vst_group(state));
    state.effects_box.append(&build_lv2_group(state));
}

// ---- LV2 chain -------------------------------------------------------------

fn build_lv2_group(state: &Rc<DialogState>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Effect Chain (LV2)")
        .description("Applied to this input before it reaches the monitor and stream mixes, top to bottom.")
        .build();

    let effects = state
        .deps
        .config
        .borrow()
        .channel(state.channel_id)
        .map(|c| c.effects.clone())
        .unwrap_or_default();
    let catalog = lv2::catalog();
    let count = effects.len();

    for (pos, effect) in effects.iter().enumerate() {
        let row = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(&effect.name))
            .build();

        let enable = gtk::Switch::builder()
            .active(effect.enabled)
            .valign(gtk::Align::Center)
            .tooltip_text("Enable this effect")
            .build();
        {
            let state = state.clone();
            let effect_id = effect.id;
            enable.connect_state_set(move |_, on| {
                structural_change(&state, |ch| {
                    if let Some(e) = ch.effect_mut(effect_id) {
                        e.enabled = on;
                    }
                });
                glib::Propagation::Proceed
            });
        }
        row.add_prefix(&enable);

        let controls_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        controls_box.set_valign(gtk::Align::Center);
        for (icon, tip, delta) in [
            ("go-up-symbolic", "Move up", -1i32),
            ("go-down-symbolic", "Move down", 1),
        ] {
            let btn = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .valign(gtk::Align::Center)
                .sensitive(if delta < 0 { pos > 0 } else { pos + 1 < count })
                .build();
            btn.add_css_class("flat");
            let state = state.clone();
            let effect_id = effect.id;
            btn.connect_clicked(move |_| {
                structural_change(&state, |ch| {
                    if let Some(i) = ch.effects.iter().position(|e| e.id == effect_id) {
                        let j = i as i32 + delta;
                        if j >= 0 && (j as usize) < ch.effects.len() {
                            ch.effects.swap(i, j as usize);
                        }
                    }
                });
            });
            controls_box.append(&btn);
        }
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove this effect")
            .valign(gtk::Align::Center)
            .build();
        remove.add_css_class("flat");
        {
            let state = state.clone();
            let effect_id = effect.id;
            remove.connect_clicked(move |_| {
                structural_change(&state, |ch| {
                    ch.effects.retain(|e| e.id != effect_id);
                });
            });
        }
        controls_box.append(&remove);
        row.add_suffix(&controls_box);

        match catalog.as_ref().and_then(|c| c.find(&effect.uri)) {
            Some(info) => {
                row.set_subtitle(&glib::markup_escape_text(&effect.uri));
                let mono = info.is_mono();
                for port in &info.controls {
                    let value = effect
                        .controls
                        .get(&port.symbol)
                        .copied()
                        .unwrap_or(f64::from(port.default));
                    row.add_row(&control_row(
                        state, effect.id, mono, port, value,
                    ));
                }
                if info.controls.is_empty() {
                    row.set_enable_expansion(false);
                }
            }
            None => {
                row.set_subtitle("Plugin is not installed — the effect is skipped");
                row.set_enable_expansion(false);
            }
        }
        group.add(&row);
    }

    // Add-effect entry point (or hints when nothing can be added).
    if lv2::available() {
        let add_row = adw::ButtonRow::builder()
            .title("Add Effect…")
            .start_icon_name("list-add-symbolic")
            .build();
        let state_c = state.clone();
        add_row.connect_activated(move |_| open_plugin_picker(&state_c));
        group.add(&add_row);
        if count == 0
            && catalog.as_ref().is_none_or(|c| c.plugins.is_empty())
        {
            group.set_description(Some(
                "No LV2 plugins found. Install some (e.g. the LSP plugin \
                 collection: lsp-plugins-lv2) to add effects here.",
            ));
        }
    } else {
        group.set_description(Some(
            "LV2 support requires the lilv library, which could not be \
             loaded. Install lilv to add effects here.",
        ));
    }

    group
}

/// One parameter row: a switch for toggle ports, otherwise a slider.
fn control_row(
    state: &Rc<DialogState>,
    effect_id: u64,
    mono: bool,
    port: &lv2::ControlPort,
    value: f64,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&port.name))
        .build();
    let symbol = port.symbol.clone();
    if port.toggled {
        let sw = gtk::Switch::builder()
            .active(value > 0.5)
            .valign(gtk::Align::Center)
            .build();
        let state = state.clone();
        sw.connect_state_set(move |_, on| {
            control_change(&state, effect_id, mono, &symbol, if on { 1.0 } else { 0.0 });
            glib::Propagation::Proceed
        });
        row.add_suffix(&sw);
        row.set_activatable_widget(Some(&sw));
    } else {
        let min = f64::from(port.min);
        let max = f64::from(port.max);
        let step = if port.integer {
            1.0
        } else {
            (max - min) / 200.0
        };
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
        scale.set_value(value.clamp(min, max));
        scale.set_width_request(200);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_digits(if port.integer { 0 } else { 2 });
        scale.set_valign(gtk::Align::Center);
        let state = state.clone();
        scale.connect_value_changed(move |s| {
            control_change(&state, effect_id, mono, &symbol, s.value());
        });
        row.add_suffix(&scale);
    }
    row
}

/// Persist a control value and (debounced) push it into the running chain.
fn control_change(
    state: &Rc<DialogState>,
    effect_id: u64,
    mono: bool,
    symbol: &str,
    value: f64,
) {
    {
        let mut cfg = state.deps.config.borrow_mut();
        if let Some(ch) = cfg.channel_mut(state.channel_id)
            && let Some(e) = ch.effect_mut(effect_id) {
                e.controls.insert(symbol.to_string(), value);
            }
    }
    (state.deps.on_control)(state.channel_id);

    if let Some(old) = state.control_debounce.borrow_mut().take() {
        old.remove();
    }
    let state_c = state.clone();
    let symbol = symbol.to_string();
    let source = glib::timeout_add_local_once(Duration::from_millis(80), move || {
        *state_c.control_debounce.borrow_mut() = None;
        state_c.deps.manager.set_effect_control(
            state_c.channel_id,
            effect_id,
            mono,
            &symbol,
            value,
        );
    });
    *state.control_debounce.borrow_mut() = Some(source);
}

/// Apply `f` to the channel config, then rebuild routing and this dialog.
fn structural_change(
    state: &Rc<DialogState>,
    f: impl FnOnce(&mut crate::config::ChannelConfig),
) {
    {
        let mut cfg = state.deps.config.borrow_mut();
        let Some(ch) = cfg.channel_mut(state.channel_id) else {
            return;
        };
        f(ch);
    }
    (state.deps.on_structure)(state.channel_id);
    // Deferred: the change may originate from a widget this rebuild removes.
    let state = state.clone();
    glib::idle_add_local_once(move || rebuild_groups(&state));
}

// ---- Plugin picker -----------------------------------------------------------

fn open_plugin_picker(state: &Rc<DialogState>) {
    let Some(catalog) = lv2::catalog() else {
        return;
    };

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search plugins…")
        .hexpand(true)
        .build();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let picker = adw::Dialog::builder()
        .title("Add Effect")
        .content_width(480)
        .content_height(600)
        .build();

    for plugin in &catalog.plugins {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&plugin.name))
            .subtitle(glib::markup_escape_text(&plugin.uri))
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
        let state = state.clone();
        let picker = picker.clone();
        let uri = plugin.uri.clone();
        let name = plugin.name.clone();
        row.connect_activated(move |_| {
            structural_change(&state, |ch| {
                ch.add_effect(&uri, &name);
            });
            picker.close();
        });
        list.append(&row);
    }

    {
        let search = search.clone();
        list.set_filter_func(move |row| {
            let text = search.text().to_lowercase();
            if text.is_empty() {
                return true;
            }
            let Some(row) = row.downcast_ref::<adw::ActionRow>() else {
                return true;
            };
            row.title().to_lowercase().contains(&text)
                || row.subtitle().is_some_and(|s| s.to_lowercase().contains(&text))
        });
    }
    {
        let list = list.clone();
        search.connect_search_changed(move |_| list.invalidate_filter());
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&search);
    content.append(&list);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .vexpand(true)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    picker.set_child(Some(&toolbar));
    picker.present(Some(&state.dialog));
}

// ---- VST rack ----------------------------------------------------------------

fn build_vst_group(state: &Rc<DialogState>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("VST Rack")
        .description(
            "VST2/VST3 plugins from your plugin folders, processed before \
             the LV2 chain.",
        )
        .build();
    let available = vst::available();

    let plugins = state
        .deps
        .config
        .borrow()
        .channel(state.channel_id)
        .map(|c| c.vst_plugins.clone())
        .unwrap_or_default();
    let runtime = state.deps.manager.vst_runtime(state.channel_id);
    let count = plugins.len();

    for (pos, plugin) in plugins.iter().enumerate() {
        let row = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(&plugin.name))
            .build();

        let enable = gtk::Switch::builder()
            .active(plugin.enabled)
            .valign(gtk::Align::Center)
            .tooltip_text("Enable this plugin")
            .build();
        {
            let state = state.clone();
            let vst_id = plugin.id;
            enable.connect_state_set(move |_, on| {
                structural_change(&state, |ch| {
                    if let Some(p) = ch.vst_mut(vst_id) {
                        p.enabled = on;
                    }
                });
                glib::Propagation::Proceed
            });
        }
        row.add_prefix(&enable);

        let runtime_entry = runtime.iter().find(|r| r.cfg_id == plugin.id);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        buttons.set_valign(gtk::Align::Center);
        if plugin.enabled
            && runtime_entry
                .is_some_and(|rt| matches!(rt.state, VstState::Loaded) && rt.has_ui)
        {
            let ui_btn = gtk::Button::builder()
                .icon_name("window-new-symbolic")
                .tooltip_text("Open the plugin's own window")
                .valign(gtk::Align::Center)
                .build();
            ui_btn.add_css_class("flat");
            let state = state.clone();
            let vst_id = plugin.id;
            ui_btn.connect_clicked(move |_| {
                state.deps.manager.show_vst_ui(state.channel_id, vst_id);
            });
            buttons.append(&ui_btn);
        }
        for (icon, tip, delta) in [
            ("go-up-symbolic", "Move up", -1i32),
            ("go-down-symbolic", "Move down", 1),
        ] {
            let btn = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .valign(gtk::Align::Center)
                .sensitive(if delta < 0 { pos > 0 } else { pos + 1 < count })
                .build();
            btn.add_css_class("flat");
            let state = state.clone();
            let vst_id = plugin.id;
            btn.connect_clicked(move |_| {
                structural_change(&state, |ch| {
                    if let Some(i) = ch.vst_plugins.iter().position(|p| p.id == vst_id) {
                        let j = i as i32 + delta;
                        if j >= 0 && (j as usize) < ch.vst_plugins.len() {
                            ch.vst_plugins.swap(i, j as usize);
                        }
                    }
                });
            });
            buttons.append(&btn);
        }
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove this plugin")
            .valign(gtk::Align::Center)
            .build();
        remove.add_css_class("flat");
        {
            let state = state.clone();
            let vst_id = plugin.id;
            remove.connect_clicked(move |_| {
                structural_change(&state, |ch| {
                    ch.vst_plugins.retain(|p| p.id != vst_id);
                });
            });
        }
        buttons.append(&remove);
        row.add_suffix(&buttons);

        if !plugin.enabled {
            row.set_subtitle("Bypassed");
            row.set_enable_expansion(false);
        } else {
            match runtime_entry {
                Some(rt) => match &rt.state {
                    VstState::Loaded => {
                        if rt.params.is_empty() {
                            row.set_subtitle("No editable parameters");
                            row.set_enable_expansion(false);
                        }
                        for param in &rt.params {
                            let value = plugin
                                .params
                                .get(&param.index.to_string())
                                .copied()
                                .unwrap_or(param.value);
                            row.add_row(&vst_param_row(state, plugin.id, param, value));
                        }
                    }
                    VstState::Loading => {
                        row.set_subtitle("Loading…");
                        row.set_enable_expansion(false);
                    }
                    VstState::Failed(e) => {
                        row.set_subtitle(&glib::markup_escape_text(e));
                        row.set_enable_expansion(false);
                    }
                },
                None => {
                    row.set_subtitle("Starting rack…");
                    row.set_enable_expansion(false);
                }
            }
        }
        group.add(&row);
    }

    if available {
        let add_row = adw::ButtonRow::builder()
            .title("Add VST Plugin…")
            .start_icon_name("list-add-symbolic")
            .build();
        let state_c = state.clone();
        add_row.connect_activated(move |_| open_vst_picker(&state_c));
        group.add(&add_row);
    } else {
        group.set_description(Some(
            "VST hosting requires Carla's engine and Python. Install the \
             “Carla” package to add VST plugins here.",
        ));
    }

    group
}

/// One VST parameter row: a switch for toggle parameters, otherwise a slider.
fn vst_param_row(
    state: &Rc<DialogState>,
    vst_id: u64,
    param: &VstParamInfo,
    value: f64,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&param.name))
        .build();
    let index = param.index;
    if param.toggled {
        let sw = gtk::Switch::builder()
            .active(value > 0.5)
            .valign(gtk::Align::Center)
            .build();
        {
            let state = state.clone();
            sw.connect_state_set(move |_, on| {
                vst_param_change(&state, vst_id, index, if on { 1.0 } else { 0.0 });
                glib::Propagation::Proceed
            });
        }
        row.add_suffix(&sw);
        row.set_activatable_widget(Some(&sw));
        state
            .vst_widgets
            .borrow_mut()
            .insert((vst_id, index), VstParamWidget::Switch(sw));
    } else {
        let min = param.min;
        let max = if param.max > param.min {
            param.max
        } else {
            param.min + 1.0
        };
        let step = if param.integer { 1.0 } else { (max - min) / 200.0 };
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
        scale.set_value(value.clamp(min, max));
        scale.set_width_request(200);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_digits(if param.integer { 0 } else { 2 });
        scale.set_valign(gtk::Align::Center);
        {
            let state = state.clone();
            scale.connect_value_changed(move |s| {
                vst_param_change(&state, vst_id, index, s.value());
            });
        }
        row.add_suffix(&scale);
        state
            .vst_widgets
            .borrow_mut()
            .insert((vst_id, index), VstParamWidget::Scale(scale));
    }
    row
}

/// Persist a VST parameter and (debounced) push it into the running rack.
fn vst_param_change(state: &Rc<DialogState>, vst_id: u64, index: u32, value: f64) {
    if state.syncing.get() {
        // The widget is being moved to a value that came FROM the engine
        // (edited in the plugin's own window); don't send it back.
        return;
    }
    {
        let mut cfg = state.deps.config.borrow_mut();
        if let Some(ch) = cfg.channel_mut(state.channel_id)
            && let Some(p) = ch.vst_mut(vst_id) {
                p.params.insert(index.to_string(), value);
            }
    }
    (state.deps.on_control)(state.channel_id);

    if let Some(old) = state.control_debounce.borrow_mut().take() {
        old.remove();
    }
    let state_c = state.clone();
    let source = glib::timeout_add_local_once(Duration::from_millis(80), move || {
        *state_c.control_debounce.borrow_mut() = None;
        state_c
            .deps
            .manager
            .set_vst_param(state_c.channel_id, vst_id, index, value);
    });
    *state.control_debounce.borrow_mut() = Some(source);
}

fn open_vst_picker(state: &Rc<DialogState>) {
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search VST plugins…")
        .hexpand(true)
        .build();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let picker = adw::Dialog::builder()
        .title("Add VST Plugin")
        .content_width(480)
        .content_height(600)
        .build();

    // Discovery probes every new binary in a helper process; the first run
    // can take a while, so scan off the main loop with a placeholder row.
    let scanning = adw::ActionRow::builder()
        .title("Scanning plugin folders…")
        .subtitle("First scan probes every plugin binary; later scans are cached.")
        .build();
    let spinner = adw::Spinner::new();
    spinner.set_valign(gtk::Align::Center);
    scanning.add_prefix(&spinner);
    list.append(&scanning);

    let handle = gio::spawn_blocking(vst::scan);
    {
        let list = list.clone();
        let picker = picker.clone();
        let state = state.clone();
        glib::MainContext::default().spawn_local(async move {
            let entries = handle.await.unwrap_or_default();
            list.remove(&scanning);
            if entries.is_empty() {
                let row = adw::ActionRow::builder()
                    .title("No VST Plugins Found")
                    .subtitle(
                        "Searched ~/vst, ~/.vst, ~/.vst3, the system vst/vst3 \
                         folders, and $VST_PATH/$VST3_PATH.",
                    )
                    .build();
                list.append(&row);
            }
            for entry in &entries {
                let subtitle = format!(
                    "{} — {}",
                    entry.format.as_str().to_uppercase(),
                    entry.path
                );
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&entry.name))
                    .subtitle(glib::markup_escape_text(&subtitle))
                    .activatable(true)
                    .build();
                row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
                let state = state.clone();
                let picker = picker.clone();
                let entry = entry.clone();
                row.connect_activated(move |_| {
                    structural_change(&state, |ch| {
                        ch.add_vst(&entry);
                    });
                    picker.close();
                });
                list.append(&row);
            }
        });
    }

    {
        let search = search.clone();
        list.set_filter_func(move |row| {
            let text = search.text().to_lowercase();
            if text.is_empty() {
                return true;
            }
            let Some(row) = row.downcast_ref::<adw::ActionRow>() else {
                return true;
            };
            row.title().to_lowercase().contains(&text)
                || row.subtitle().is_some_and(|s| s.to_lowercase().contains(&text))
        });
    }
    {
        let list = list.clone();
        search.connect_search_changed(move |_| list.invalidate_filter());
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&search);
    content.append(&list);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .vexpand(true)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    picker.set_child(Some(&toolbar));
    picker.present(Some(&state.dialog));
}
