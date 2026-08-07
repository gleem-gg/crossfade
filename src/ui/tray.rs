//! Tray indicator: a StatusNotifierItem plus its DBusMenu, exported on the
//! D-Bus connection the GApplication already owns.
//!
//! GTK4 dropped `GtkStatusIcon`, so SNI is the only way left. Both interfaces
//! are served by hand through GDBus rather than via a helper crate: it keeps
//! the app free of a second D-Bus stack and a worker thread, exactly like the
//! `gg.gleem.Crossfade.Mixer1` control API next door in `dbus.rs`.
//!
//! Everything is best-effort. Without a `StatusNotifierWatcher` on the bus —
//! plain GNOME, no AppIndicator extension — nothing is registered and the app
//! runs as before; the watcher is watched, so enabling the extension (or
//! restarting the shell) later brings the indicator up on its own.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::audio::Mix;
use crate::config::Config;

use super::window::ControlAction;

const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";
const MENU_PATH: &str = "/MenuBar";
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

/// Themed icon names: standard freedesktop names so every panel can recolor
/// them to its own foreground, instead of a private pixmap that would vanish
/// on a panel of the wrong shade.
const ICON_LIVE: &str = "audio-input-microphone-symbolic";
const ICON_MUTED: &str = "microphone-sensitivity-muted-symbolic";

const ID_ROOT: i32 = 0;
const ID_MUTE: i32 = 1;
const ID_SEPARATOR: i32 = 2;
const ID_SHOW: i32 = 3;
const ID_QUIT: i32 = 4;

pub struct TrayDeps {
    pub config: Rc<std::cell::RefCell<Config>>,
    pub perform: Rc<dyn Fn(ControlAction)>,
    /// Bring the (possibly hidden) window back.
    pub present: Rc<dyn Fn()>,
    /// The `app.quit` path: save, unload the server state, exit.
    pub quit: Rc<dyn Fn()>,
}

const ITEM_XML: &str = r#"
<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="AttentionIconName" type="s" access="read"/>
    <property name="OverlayIconName" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <method name="Activate">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="SecondaryActivate">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="ContextMenu">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="Scroll">
      <arg type="i" name="delta" direction="in"/>
      <arg type="s" name="orientation" direction="in"/>
    </method>
    <signal name="NewIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewTitle"/>
    <signal name="NewStatus">
      <arg type="s" name="status"/>
    </signal>
  </interface>
</node>
"#;

const MENU_XML: &str = r#"
<node>
  <interface name="com.canonical.dbusmenu">
    <property name="Version" type="u" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <method name="GetLayout">
      <arg type="i" name="parentId" direction="in"/>
      <arg type="i" name="recursionDepth" direction="in"/>
      <arg type="as" name="propertyNames" direction="in"/>
      <arg type="u" name="revision" direction="out"/>
      <arg type="(ia{sv}av)" name="layout" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg type="ai" name="ids" direction="in"/>
      <arg type="as" name="propertyNames" direction="in"/>
      <arg type="a(ia{sv})" name="properties" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg type="i" name="id" direction="in"/>
      <arg type="s" name="name" direction="in"/>
      <arg type="v" name="value" direction="out"/>
    </method>
    <method name="Event">
      <arg type="i" name="id" direction="in"/>
      <arg type="s" name="eventId" direction="in"/>
      <arg type="v" name="data" direction="in"/>
      <arg type="u" name="timestamp" direction="in"/>
    </method>
    <method name="EventGroup">
      <arg type="a(isvu)" name="events" direction="in"/>
      <arg type="ai" name="idErrors" direction="out"/>
    </method>
    <method name="AboutToShow">
      <arg type="i" name="id" direction="in"/>
      <arg type="b" name="needUpdate" direction="out"/>
    </method>
    <method name="AboutToShowGroup">
      <arg type="ai" name="ids" direction="in"/>
      <arg type="ai" name="updatesNeeded" direction="out"/>
      <arg type="ai" name="idErrors" direction="out"/>
    </method>
    <signal name="LayoutUpdated">
      <arg type="u" name="revision"/>
      <arg type="i" name="parent"/>
    </signal>
    <signal name="ItemsPropertiesUpdated">
      <arg type="a(ia{sv})" name="updatedProps"/>
      <arg type="a(ias)" name="removedProps"/>
    </signal>
  </interface>
</node>
"#;

/// What the indicator shows: the microphone channel's state on the mixes the
/// audience hears.
#[derive(Clone, Copy, PartialEq, Eq)]
struct MicState {
    /// `None` when the config has no usable microphone channel.
    channel: Option<u64>,
    muted: bool,
}

struct Tray {
    connection: gio::DBusConnection,
    deps: TrayDeps,
    /// Bus name we own and hand to the watcher.
    name: String,
    revision: Cell<u32>,
    /// State the last emitted change was about, so repeated refreshes stay
    /// silent on the bus.
    shown: Cell<Option<MicState>>,
    /// The label of the mute item, kept beside `shown` for the same reason.
    label: std::cell::RefCell<String>,
    name_owned: Cell<bool>,
    watcher_present: Cell<bool>,
}

/// Export the indicator; returns a refresh hook to call after mixer changes,
/// or `None` when there is no session bus.
pub fn register(application: &adw::Application, deps: TrayDeps) -> Option<Rc<dyn Fn()>> {
    let connection = application.dbus_connection()?;
    let item = gio::DBusNodeInfo::for_xml(ITEM_XML)
        .ok()?
        .lookup_interface(ITEM_IFACE)?;
    let menu = gio::DBusNodeInfo::for_xml(MENU_XML)
        .ok()?
        .lookup_interface(MENU_IFACE)?;

    let tray = Rc::new(Tray {
        connection: connection.clone(),
        deps,
        // The spec's naming scheme; the trailing counter exists for hosts
        // that key items by name, one process being able to own several.
        name: format!("org.kde.StatusNotifierItem-{}-1", std::process::id()),
        revision: Cell::new(1),
        shown: Cell::new(None),
        label: std::cell::RefCell::new(String::new()),
        name_owned: Cell::new(false),
        watcher_present: Cell::new(false),
    });
    tray.label.replace(tray.mute_label());
    tray.shown.set(Some(tray.state()));

    let registered = connection
        .register_object(ITEM_PATH, &item)
        .method_call({
            let tray = tray.clone();
            move |_, _, _, _, method, params, invocation| {
                tray.item_call(method, &params, invocation);
            }
        })
        .property({
            let tray = tray.clone();
            move |_, _, _, _, property| tray.item_property(property)
        })
        .build();
    if let Err(e) = registered {
        eprintln!("gleem-crossfade: could not export the tray item: {e}");
        return None;
    }
    let registered = connection
        .register_object(MENU_PATH, &menu)
        .method_call({
            let tray = tray.clone();
            move |_, _, _, _, method, params, invocation| {
                tray.menu_call(method, &params, invocation);
            }
        })
        .property({
            let tray = tray.clone();
            move |_, _, _, _, property| tray.menu_property(property)
        })
        .build();
    if let Err(e) = registered {
        eprintln!("gleem-crossfade: could not export the tray menu: {e}");
        return None;
    }

    gio::bus_own_name_on_connection(
        &connection,
        &tray.name,
        gio::BusNameOwnerFlags::NONE,
        {
            let tray = tray.clone();
            move |_, _| {
                tray.name_owned.set(true);
                tray.register_with_watcher();
            }
        },
        {
            let tray = tray.clone();
            move |_, _| tray.name_owned.set(false)
        },
    );
    // Watching (instead of registering once) is what makes a shell restart or
    // a later-enabled AppIndicator extension pick the item up.
    gio::bus_watch_name_on_connection(
        &connection,
        WATCHER_NAME,
        gio::BusNameWatcherFlags::NONE,
        {
            let tray = tray.clone();
            move |_, _, _| {
                tray.watcher_present.set(true);
                tray.register_with_watcher();
            }
        },
        {
            let tray = tray.clone();
            move |_, _| tray.watcher_present.set(false)
        },
    );

    Some(Rc::new(move || tray.refresh()))
}

/// The microphone channel: the permanent one named "Microphone", else the
/// first permanent channel fed by a capture device — the same rule the setup
/// assistant uses.
fn mic_channel(config: &Config) -> Option<&crate::config::ChannelConfig> {
    super::setup::mic_channel(config)
}

impl Tray {
    fn state(&self) -> MicState {
        let config = self.deps.config.borrow();
        match mic_channel(&config) {
            Some(channel) => MicState {
                channel: Some(channel.id),
                muted: channel.stream_muted,
            },
            None => MicState {
                channel: None,
                muted: false,
            },
        }
    }

    fn mute_label(&self) -> String {
        let config = self.deps.config.borrow();
        match mic_channel(&config) {
            // Underscores are mnemonic markers in a DBusMenu label.
            Some(channel) => format!("Mute {}", channel.name.replace('_', "__")),
            None => "Mute Microphone".to_string(),
        }
    }

    fn icon_name(&self) -> &'static str {
        if self.state().muted {
            ICON_MUTED
        } else {
            ICON_LIVE
        }
    }

    fn tooltip_text(&self) -> String {
        let state = self.state();
        if state.channel.is_none() {
            return "No microphone channel".to_string();
        }
        if state.muted {
            "Microphone muted — the stream cannot hear you".to_string()
        } else {
            "Microphone live on the stream mix".to_string()
        }
    }

    /// Announce a changed mute state to the host. Cheap to call on every
    /// mixer change: nothing goes on the bus unless something visible moved.
    fn refresh(&self) {
        let state = self.state();
        let label = self.mute_label();
        if self.shown.get() == Some(state) && *self.label.borrow() == label {
            return;
        }
        self.shown.set(Some(state));
        self.label.replace(label);
        let _ = self
            .connection
            .emit_signal(None, ITEM_PATH, ITEM_IFACE, "NewIcon", None);
        let _ = self
            .connection
            .emit_signal(None, ITEM_PATH, ITEM_IFACE, "NewToolTip", None);
        // Bumping the revision is the portable way to get every host to
        // re-read the menu; per-property updates are optional in DBusMenu.
        let revision = self.revision.get().wrapping_add(1).max(1);
        self.revision.set(revision);
        let _ = self.connection.emit_signal(
            None,
            MENU_PATH,
            MENU_IFACE,
            "LayoutUpdated",
            Some(&(revision, ID_ROOT).to_variant()),
        );
    }

    fn register_with_watcher(&self) {
        if !self.name_owned.get() || !self.watcher_present.get() {
            return;
        }
        self.connection.call(
            Some(WATCHER_NAME),
            WATCHER_PATH,
            WATCHER_NAME,
            "RegisterStatusNotifierItem",
            Some(&(self.name.clone(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |result| {
                if let Err(e) = result {
                    eprintln!("gleem-crossfade: the tray host refused the indicator: {e}");
                }
            },
        );
    }

    // ---- StatusNotifierItem ------------------------------------------------

    fn item_property(&self, property: &str) -> glib::Variant {
        match property {
            "Category" => "ApplicationStatus".to_variant(),
            "Id" => crate::APP_ID.to_variant(),
            "Title" => "Gleem Crossfade".to_variant(),
            "Status" => "Active".to_variant(),
            "IconName" => self.icon_name().to_variant(),
            "ToolTip" => (
                self.icon_name(),
                Vec::<(i32, i32, Vec<u8>)>::new(),
                "Gleem Crossfade",
                self.tooltip_text(),
            )
                .to_variant(),
            // False: a left click activates (presents the window) instead of
            // opening the menu, which is what the spec reserves for a
            // right click.
            "ItemIsMenu" => false.to_variant(),
            "Menu" => object_path(MENU_PATH),
            _ => String::new().to_variant(),
        }
    }

    fn item_call(
        &self,
        method: &str,
        _params: &glib::Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        match method {
            "Activate" | "SecondaryActivate" => {
                (self.deps.present)();
                invocation.return_value(None);
            }
            // The host draws the menu itself from the DBusMenu; nothing to do.
            "ContextMenu" | "Scroll" => invocation.return_value(None),
            _ => invocation.return_error(gio::DBusError::UnknownMethod, "no such method"),
        }
    }

    // ---- DBusMenu -----------------------------------------------------------

    fn menu_property(&self, property: &str) -> glib::Variant {
        match property {
            "Version" => 3u32.to_variant(),
            "Status" => "normal".to_variant(),
            "TextDirection" => "ltr".to_variant(),
            "IconThemePath" => Vec::<String>::new().to_variant(),
            _ => String::new().to_variant(),
        }
    }

    /// Properties of one menu entry, as the host expects them.
    fn item_properties(&self, id: i32) -> HashMap<String, glib::Variant> {
        let mut props: HashMap<String, glib::Variant> = HashMap::new();
        let mut set = |key: &str, value: glib::Variant| {
            props.insert(key.to_string(), value);
        };
        match id {
            ID_ROOT => {
                set("children-display", "submenu".to_variant());
            }
            ID_MUTE => {
                let state = self.state();
                set("label", self.label.borrow().as_str().to_variant());
                set("toggle-type", "checkmark".to_variant());
                set("toggle-state", i32::from(state.muted).to_variant());
                set("enabled", state.channel.is_some().to_variant());
            }
            ID_SEPARATOR => {
                set("type", "separator".to_variant());
            }
            ID_SHOW => {
                set("label", "Show Crossfade".to_variant());
                set("icon-name", "audio-card-symbolic".to_variant());
            }
            ID_QUIT => {
                set("label", "Quit".to_variant());
                set("icon-name", "application-exit-symbolic".to_variant());
            }
            _ => {}
        }
        props
    }

    /// One layout node: `(id, properties, children)`.
    fn layout_node(&self, id: i32, children: &[i32]) -> glib::Variant {
        let children: Vec<glib::Variant> = children
            .iter()
            .map(|child| glib::Variant::from_variant(&self.layout_node(*child, &[])))
            .collect();
        glib::Variant::tuple_from_iter([
            id.to_variant(),
            self.item_properties(id).to_variant(),
            glib::Variant::array_from_iter::<glib::Variant>(children),
        ])
    }

    fn menu_call(
        &self,
        method: &str,
        params: &glib::Variant,
        invocation: gio::DBusMethodInvocation,
    ) {
        match method {
            "GetLayout" => {
                let parent = params.child_value(0).get::<i32>().unwrap_or(ID_ROOT);
                let layout = if parent == ID_ROOT {
                    self.layout_node(ID_ROOT, &[ID_MUTE, ID_SEPARATOR, ID_SHOW, ID_QUIT])
                } else {
                    self.layout_node(parent, &[])
                };
                invocation.return_value(Some(&glib::Variant::tuple_from_iter([
                    self.revision.get().to_variant(),
                    layout,
                ])));
            }
            "GetGroupProperties" => {
                let ids: Vec<i32> = params
                    .child_value(0)
                    .get::<Vec<i32>>()
                    .filter(|ids| !ids.is_empty())
                    .unwrap_or_else(|| vec![ID_MUTE, ID_SEPARATOR, ID_SHOW, ID_QUIT]);
                let rows: Vec<glib::Variant> = ids
                    .iter()
                    .map(|id| {
                        glib::Variant::tuple_from_iter([
                            id.to_variant(),
                            self.item_properties(*id).to_variant(),
                        ])
                    })
                    .collect();
                let array = glib::Variant::array_from_iter_with_type(
                    glib::VariantTy::new("(ia{sv})").expect("valid type"),
                    rows,
                );
                invocation.return_value(Some(&glib::Variant::tuple_from_iter([array])));
            }
            "GetProperty" => {
                let id = params.child_value(0).get::<i32>().unwrap_or(ID_ROOT);
                let name = params.child_value(1).get::<String>().unwrap_or_default();
                let value = self
                    .item_properties(id)
                    .remove(&name)
                    .unwrap_or_else(|| String::new().to_variant());
                invocation.return_value(Some(&glib::Variant::tuple_from_iter([
                    glib::Variant::from_variant(&value),
                ])));
            }
            "Event" => {
                let id = params.child_value(0).get::<i32>().unwrap_or(-1);
                let event = params.child_value(1).get::<String>().unwrap_or_default();
                invocation.return_value(None);
                if event == "clicked" {
                    self.activate_item(id);
                }
            }
            "EventGroup" => {
                let events = params.child_value(0);
                let mut clicked = Vec::new();
                for i in 0..events.n_children() {
                    let event = events.child_value(i);
                    let id = event.child_value(0).get::<i32>().unwrap_or(-1);
                    let kind = event.child_value(1).get::<String>().unwrap_or_default();
                    if kind == "clicked" {
                        clicked.push(id);
                    }
                }
                invocation.return_value(Some(&(Vec::<i32>::new(),).to_variant()));
                for id in clicked {
                    self.activate_item(id);
                }
            }
            "AboutToShow" => {
                // The menu is rebuilt from live config on every read, so a
                // host never needs to be told to refresh first.
                invocation.return_value(Some(&(false,).to_variant()));
            }
            "AboutToShowGroup" => {
                invocation
                    .return_value(Some(&(Vec::<i32>::new(), Vec::<i32>::new()).to_variant()));
            }
            _ => invocation.return_error(gio::DBusError::UnknownMethod, "no such method"),
        }
    }

    fn activate_item(&self, id: i32) {
        match id {
            ID_MUTE => {
                let state = self.state();
                let Some(channel) = state.channel else {
                    return;
                };
                let muted = !state.muted;
                // Mute towards everyone who is listening. The monitor mix is
                // deliberately left alone: it is the user's own headphones,
                // and it is routinely muted there already.
                (self.deps.perform)(ControlAction::SetChannelMute {
                    id: channel,
                    mix: Mix::Stream,
                    muted: Some(muted),
                });
                if self.deps.config.borrow().vod_mix_enabled {
                    (self.deps.perform)(ControlAction::SetChannelMute {
                        id: channel,
                        mix: Mix::Vod,
                        muted: Some(muted),
                    });
                }
                self.refresh();
            }
            ID_SHOW => (self.deps.present)(),
            ID_QUIT => (self.deps.quit)(),
            _ => {}
        }
    }
}

fn object_path(path: &str) -> glib::Variant {
    glib::Variant::parse(Some(glib::VariantTy::OBJECT_PATH), &format!("'{path}'"))
        .expect("valid object path")
}
