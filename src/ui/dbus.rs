//! Session-bus control API: the `gg.gleem.Crossfade.Mixer1` interface at
//! `/gg/gleem/Crossfade/Mixer`, exported on the D-Bus connection the
//! GApplication already owns. External integrations — hotkey daemons,
//! stream-deck software, desktop widgets, plain scripts — drive the mixer
//! through it; every method funnels into the same `ControlAction` core the
//! MIDI dispatch uses. The parameterless `StateChanged` signal fires
//! (debounced) after any mixer change; clients re-query via `GetVolumes`.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::audio::Mix;
use crate::config::{Assignment, Config};

use super::window::ControlAction;

pub struct DbusDeps {
    pub config: Rc<RefCell<Config>>,
    pub perform: Rc<dyn Fn(ControlAction)>,
}

const IFACE: &str = "gg.gleem.Crossfade.Mixer1";
const PATH: &str = "/gg/gleem/Crossfade/Mixer";

const INTROSPECTION_XML: &str = r#"
<node>
  <interface name="gg.gleem.Crossfade.Mixer1">
    <method name="ListChannels">
      <arg type="a(tss)" name="channels" direction="out"/>
    </method>
    <method name="GetVolumes">
      <arg type="s" name="json" direction="out"/>
    </method>
    <method name="SetChannelVolume">
      <arg type="t" name="id" direction="in"/>
      <arg type="s" name="mix" direction="in"/>
      <arg type="d" name="value" direction="in"/>
    </method>
    <method name="SetChannelMute">
      <arg type="t" name="id" direction="in"/>
      <arg type="s" name="mix" direction="in"/>
      <arg type="b" name="muted" direction="in"/>
    </method>
    <method name="ToggleChannelMute">
      <arg type="t" name="id" direction="in"/>
      <arg type="s" name="mix" direction="in"/>
      <arg type="b" name="muted" direction="out"/>
    </method>
    <method name="SetMasterVolume">
      <arg type="s" name="mix" direction="in"/>
      <arg type="d" name="value" direction="in"/>
    </method>
    <method name="SetMasterMute">
      <arg type="s" name="mix" direction="in"/>
      <arg type="b" name="muted" direction="in"/>
    </method>
    <method name="ToggleMasterMute">
      <arg type="s" name="mix" direction="in"/>
      <arg type="b" name="muted" direction="out"/>
    </method>
    <method name="ListMidiProfiles">
      <arg type="a(ts)" name="profiles" direction="out"/>
    </method>
    <method name="SelectMidiProfile">
      <arg type="t" name="id" direction="in"/>
    </method>
    <signal name="StateChanged"/>
  </interface>
</node>
"#;

/// Export the interface; returns the StateChanged emitter, or None when the
/// app is not on a session bus (or registration failed).
pub fn register(application: &adw::Application, deps: DbusDeps) -> Option<Rc<dyn Fn()>> {
    let connection = application.dbus_connection()?;
    let node = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML).ok()?;
    let interface = node.lookup_interface(IFACE)?;
    let deps = Rc::new(deps);
    let registered = connection
        .register_object(PATH, &interface)
        .method_call(move |_conn, _sender, _path, _iface, method, params, invocation| {
            handle(&deps, method, params, invocation);
        })
        .build();
    if let Err(e) = registered {
        eprintln!("gleem-crossfade: could not register the D-Bus control interface: {e}");
        return None;
    }
    let connection = connection.clone();
    Some(Rc::new(move || {
        let _ = connection.emit_signal(None, PATH, IFACE, "StateChanged", None);
    }))
}

fn channel_muted(cfg: &Config, id: u64, mix: Mix) -> bool {
    cfg.channel(id)
        .map(|c| match mix {
            Mix::Monitor => c.monitor_muted,
            Mix::Stream => c.stream_muted,
            Mix::Vod => c.vod_muted,
        })
        .unwrap_or(false)
}

fn master_muted(cfg: &Config, mix: Mix) -> bool {
    match mix {
        Mix::Monitor => cfg.master.monitor_muted,
        Mix::Stream => cfg.master.stream_muted,
        Mix::Vod => cfg.master.vod_muted,
    }
}

fn handle(
    deps: &Rc<DbusDeps>,
    method: &str,
    params: glib::Variant,
    invocation: gio::DBusMethodInvocation,
) {
    let invalid = |invocation: gio::DBusMethodInvocation, msg: &str| {
        invocation.return_error(gio::DBusError::InvalidArgs, msg);
    };
    let parse_mix = |key: &str| Mix::from_key(key);
    match method {
        "ListChannels" => {
            let cfg = deps.config.borrow();
            let rows: Vec<(u64, String, String)> = cfg
                .channels
                .iter()
                .map(|c| {
                    let kind = match &c.assignment {
                        Some(Assignment::Source { .. }) => "source",
                        Some(Assignment::App { .. }) => "app",
                        Some(Assignment::Virtual) => "virtual",
                        None => "none",
                    };
                    (c.id, c.name.clone(), kind.to_string())
                })
                .collect();
            invocation.return_value(Some(&(rows,).to_variant()));
        }
        "GetVolumes" => {
            let cfg = deps.config.borrow();
            let mix_state = |volume: f64, muted: bool| {
                serde_json::json!({ "volume": volume, "muted": muted })
            };
            let json = serde_json::json!({
                "channels": cfg.channels.iter().map(|c| serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "color": c.color,
                    "monitor": mix_state(c.monitor_volume, c.monitor_muted),
                    "stream": mix_state(c.stream_volume, c.stream_muted),
                    "vod": mix_state(c.vod_volume, c.vod_muted),
                })).collect::<Vec<_>>(),
                "master": {
                    "monitor": mix_state(cfg.master.monitor_volume, cfg.master.monitor_muted),
                    "stream": mix_state(cfg.master.stream_volume, cfg.master.stream_muted),
                    "vod": mix_state(cfg.master.vod_volume, cfg.master.vod_muted),
                },
                "vod_mix_enabled": cfg.vod_mix_enabled,
                "active_midi_profile": cfg.midi.active_profile,
            });
            invocation.return_value(Some(&(json.to_string(),).to_variant()));
        }
        "SetChannelVolume" => {
            let Some((id, mix, value)) = params.get::<(u64, String, f64)>() else {
                return invalid(invocation, "expected (t id, s mix, d value)");
            };
            let Some(mix) = parse_mix(&mix) else {
                return invalid(invocation, "mix must be monitor, stream or vod");
            };
            if deps.config.borrow().channel(id).is_none() {
                return invalid(invocation, "unknown channel id");
            }
            (deps.perform)(ControlAction::SetChannelVolume { id, mix, value });
            invocation.return_value(None);
        }
        "SetChannelMute" => {
            let Some((id, mix, muted)) = params.get::<(u64, String, bool)>() else {
                return invalid(invocation, "expected (t id, s mix, b muted)");
            };
            let Some(mix) = parse_mix(&mix) else {
                return invalid(invocation, "mix must be monitor, stream or vod");
            };
            if deps.config.borrow().channel(id).is_none() {
                return invalid(invocation, "unknown channel id");
            }
            (deps.perform)(ControlAction::SetChannelMute {
                id,
                mix,
                muted: Some(muted),
            });
            invocation.return_value(None);
        }
        "ToggleChannelMute" => {
            let Some((id, mix)) = params.get::<(u64, String)>() else {
                return invalid(invocation, "expected (t id, s mix)");
            };
            let Some(mix) = parse_mix(&mix) else {
                return invalid(invocation, "mix must be monitor, stream or vod");
            };
            if deps.config.borrow().channel(id).is_none() {
                return invalid(invocation, "unknown channel id");
            }
            (deps.perform)(ControlAction::SetChannelMute {
                id,
                mix,
                muted: None,
            });
            let muted = channel_muted(&deps.config.borrow(), id, mix);
            invocation.return_value(Some(&(muted,).to_variant()));
        }
        "SetMasterVolume" => {
            let Some((mix, value)) = params.get::<(String, f64)>() else {
                return invalid(invocation, "expected (s mix, d value)");
            };
            let Some(mix) = parse_mix(&mix) else {
                return invalid(invocation, "mix must be monitor, stream or vod");
            };
            (deps.perform)(ControlAction::SetMasterVolume { mix, value });
            invocation.return_value(None);
        }
        "SetMasterMute" => {
            let Some((mix, muted)) = params.get::<(String, bool)>() else {
                return invalid(invocation, "expected (s mix, b muted)");
            };
            let Some(mix) = parse_mix(&mix) else {
                return invalid(invocation, "mix must be monitor, stream or vod");
            };
            (deps.perform)(ControlAction::SetMasterMute {
                mix,
                muted: Some(muted),
            });
            invocation.return_value(None);
        }
        "ToggleMasterMute" => {
            let Some((mix,)) = params.get::<(String,)>() else {
                return invalid(invocation, "expected (s mix)");
            };
            let Some(mix) = parse_mix(&mix) else {
                return invalid(invocation, "mix must be monitor, stream or vod");
            };
            (deps.perform)(ControlAction::SetMasterMute { mix, muted: None });
            let muted = master_muted(&deps.config.borrow(), mix);
            invocation.return_value(Some(&(muted,).to_variant()));
        }
        "ListMidiProfiles" => {
            let cfg = deps.config.borrow();
            let rows: Vec<(u64, String)> = cfg
                .midi
                .profiles
                .iter()
                .map(|p| (p.id, p.name.clone()))
                .collect();
            invocation.return_value(Some(&(rows,).to_variant()));
        }
        "SelectMidiProfile" => {
            let Some((id,)) = params.get::<(u64,)>() else {
                return invalid(invocation, "expected (t id)");
            };
            if deps.config.borrow().midi.profile(id).is_none() {
                return invalid(invocation, "unknown profile id");
            }
            (deps.perform)(ControlAction::SelectMidiProfile { id });
            invocation.return_value(None);
        }
        _ => invocation.return_error(gio::DBusError::UnknownMethod, "unknown method"),
    }
}
