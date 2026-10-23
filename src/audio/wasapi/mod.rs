//! Windows backend: the WASAPI engine plus the Crossfade virtual audio
//! driver, behind the same API as the Linux `PulseManager`.
//!
//! ```text
//!  capture endpoint ─┐                         ┌─▶ monitor bus ──▶ headphones
//!  (or loopback of a ├──▶ engine thread (mix) ─┼─▶ stream bus ──▶ "Crossfade Stream Mix" (driver)
//!   "Crossfade       │                         └─▶ VOD bus    ──▶ "Crossfade VOD Mix"    (driver)
//!   Channel N")     ─┘
//! ```
//!
//! Channels that expose a device (Virtual, App, catch-all) are backed by one
//! of the driver's fixed render endpoints ("Crossfade Channel N"), recorded
//! through WASAPI loopback. App channels pin matching processes to their
//! endpoint through the per-app default-device policy — the same mechanism
//! as Windows' "App volume and device preferences".
//!
//! Everything here runs on the GTK main thread; only the engine has its own
//! (real-time) thread. A 30 Hz poll drains engine events and picks up
//! endpoint and session changes.

mod devices;
mod driver;
mod engine;
mod namer;
mod policy;
mod sessions;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

use self::devices::{Devices, Endpoint};
use self::driver::{DriverRole, MAX_SLOTS};
use self::engine::{Command, Engine, EngineEvent, InputSpec};
use self::policy::AppRouting;
use self::sessions::Sessions;
use super::{AudioEvent, Mix, SinkEntry, SourceEntry, is_elgato_wave, is_wave_xlr};
use crate::config::{self, Assignment, ChannelConfig, Config, MasterConfig};

/// Sources that record a render endpoint ("monitors") are named with this
/// prefix in front of the endpoint id.
const LOOPBACK_PREFIX: &str = "loopback:";
const POLL_INTERVAL: Duration = Duration::from_millis(33);
const SESSION_INTERVAL: Duration = Duration::from_secs(1);
/// Endpoint notifications come in bursts (and property changes can be
/// chatty); re-enumerate at most this often.
const DEVICE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Effective volume and mute of one channel's send into a mix; the stream
/// and VOD sends fold their master level in, exactly like the Linux side.
fn mix_levels(c: &ChannelConfig, m: &MasterConfig, mix: Mix) -> (f64, bool) {
    match mix {
        Mix::Monitor => (c.monitor_volume, c.monitor_muted),
        Mix::Stream => (
            c.stream_volume * m.stream_volume,
            c.stream_muted || m.stream_muted,
        ),
        Mix::Vod => (c.vod_volume * m.vod_volume, c.vod_muted || m.vod_muted),
    }
}

/// Fader position → linear gain, on PulseAudio's cubic software-volume
/// curve so the faders feel the same on both platforms.
fn gain(v: f64, muted: bool) -> f32 {
    if muted {
        0.0
    } else {
        v.clamp(0.0, 1.0).powi(3) as f32
    }
}

struct Inner {
    config: Rc<RefCell<Config>>,
    handler: Option<Rc<dyn Fn(AudioEvent)>>,
    devices: Option<Devices>,
    sessions: Option<Sessions>,
    routing: Option<AppRouting>,
    engine: Option<Engine>,
    endpoints: Vec<Endpoint>,
    default_render: Option<String>,
    default_capture: Option<String>,
    /// Live playback sessions: pid → application name.
    apps: BTreeMap<u32, String>,
    /// Processes we pinned to a Crossfade endpoint (pid → endpoint id).
    routes: HashMap<u32, String>,
    /// Endpoint names last requested from the naming service, and when.
    names_requested: Option<(Vec<(u8, String)>, Instant)>,
    poll: Option<glib::SourceId>,
    last_device_scan: Instant,
    last_session_scan: Instant,
}

#[derive(Clone)]
pub struct WasapiManager {
    inner: Rc<RefCell<Inner>>,
}

impl WasapiManager {
    pub fn new(config: Rc<RefCell<Config>>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Inner {
                config,
                handler: None,
                devices: None,
                sessions: None,
                routing: None,
                engine: None,
                endpoints: Vec::new(),
                default_render: None,
                default_capture: None,
                apps: BTreeMap::new(),
                routes: HashMap::new(),
                names_requested: None,
                poll: None,
                last_device_scan: Instant::now(),
                last_session_scan: Instant::now(),
            })),
        }
    }

    pub fn set_event_handler(&self, f: impl Fn(AudioEvent) + 'static) {
        self.inner.borrow_mut().handler = Some(Rc::new(f));
    }

    fn emit(&self, ev: AudioEvent) {
        let handler = self.inner.borrow().handler.clone();
        if let Some(h) = handler {
            glib::idle_add_local_once(move || h(ev));
        }
    }

    /// (Re)start: enumerate devices, check for the driver, start the engine
    /// and wire every channel.
    pub fn connect_server(&self) {
        self.stop_engine();
        // GTK may already have initialized the main thread's apartment
        // (OLE drag and drop); either way COM is usable afterwards.
        // SAFETY: per-thread COM initialization.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let devices = match Devices::new() {
            Ok(d) => d,
            Err(e) => {
                self.emit(AudioEvent::Failed(format!(
                    "Could not access the Windows audio system: {e}"
                )));
                return;
            }
        };
        let problem = match driver::state() {
            driver::DriverState::Running => None,
            driver::DriverState::Missing => Some(
                "The Crossfade virtual audio driver is not installed. Install it with \
                 the driver setup from the Crossfade download, then press Retry."
                    .to_string(),
            ),
            driver::DriverState::Problem(driver::PROBLEM_UNSIGNED) => Some(
                "Windows blocked the Crossfade virtual audio driver because it is not \
                 signed by Microsoft. Development builds only load in test-signing mode \
                 (see driver/README.md)."
                    .to_string(),
            ),
            driver::DriverState::Problem(code) => Some(format!(
                "The Crossfade virtual audio driver did not start (Device Manager \
                 reports problem code {code})."
            )),
        };
        if let Some(msg) = problem
            && std::env::var_os("CROSSFADE_ALLOW_NO_DRIVER").is_none()
        {
            self.emit(AudioEvent::Failed(msg));
            return;
        }
        {
            let mut inner = self.inner.borrow_mut();
            inner.devices = Some(devices);
            inner.sessions = Sessions::new().ok();
            inner.routing = AppRouting::new();
            if inner.routing.is_none() {
                eprintln!("gleem-crossfade: per-app routing is unavailable on this Windows build");
            }
            inner.engine = Some(Engine::start());
        }
        self.refresh_devices();
        self.assign_slots();
        self.adopt_stale_routes();

        let ids: Vec<u64> = self.inner.borrow().config.borrow().channels.iter().map(|c| c.id).collect();
        for id in ids {
            self.rebuild_channel(id);
        }
        self.apply_master_monitor();
        self.apply_vod_mix();
        self.setup_monitor_output();
        self.scan_sessions();
        self.start_poll();
        self.emit(AudioEvent::Ready);
        self.emit(AudioEvent::DevicesChanged);
    }

    fn stop_engine(&self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(id) = inner.poll.take() {
            id.remove();
        }
        if let Some(mut engine) = inner.engine.take() {
            engine.stop();
        }
    }

    fn send(&self, cmd: Command) {
        if let Some(engine) = &self.inner.borrow().engine {
            engine.send(cmd);
        }
    }

    fn start_poll(&self) {
        let weak = Rc::downgrade(&self.inner);
        let id = glib::timeout_add_local(POLL_INTERVAL, move || {
            let Some(inner) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            WasapiManager { inner }.poll();
            glib::ControlFlow::Continue
        });
        self.inner.borrow_mut().poll = Some(id);
    }

    fn poll(&self) {
        let events = self
            .inner
            .borrow()
            .engine
            .as_ref()
            .map(Engine::events)
            .unwrap_or_default();
        for ev in events {
            match ev {
                EngineEvent::Levels(levels) => {
                    for (target, v) in levels {
                        self.emit(AudioEvent::Level(target, v));
                    }
                }
                EngineEvent::Warning(msg) => eprintln!("gleem-crossfade: {msg}"),
            }
        }

        let now = Instant::now();
        let devices_dirty = {
            let inner = self.inner.borrow();
            now - inner.last_device_scan >= DEVICE_DEBOUNCE
                && inner.devices.as_ref().is_some_and(Devices::take_dirty)
        };
        if devices_dirty {
            self.inner.borrow_mut().last_device_scan = now;
            self.refresh_devices();
            // Device ids resolve differently now (driver endpoints appeared,
            // the default output changed, …); re-send every input. The
            // engine ignores inputs that did not change.
            let ids: Vec<u64> =
                self.inner.borrow().config.borrow().channels.iter().map(|c| c.id).collect();
            for id in ids {
                self.send_input(id);
            }
            self.setup_monitor_output();
            self.sync_endpoint_names();
            self.emit(AudioEvent::DevicesChanged);
        }

        let sessions_due = now - self.inner.borrow().last_session_scan >= SESSION_INTERVAL;
        if sessions_due {
            self.inner.borrow_mut().last_session_scan = now;
            self.scan_sessions();
        }
    }

    fn refresh_devices(&self) {
        let mut inner = self.inner.borrow_mut();
        let Some(devices) = &inner.devices else {
            return;
        };
        let endpoints = devices.enumerate();
        let default_render = devices.default_endpoint(true);
        let default_capture = devices.default_endpoint(false);
        inner.endpoints = endpoints;
        inner.default_render = default_render;
        inner.default_capture = default_capture;
    }

    // ---- Channel wiring -------------------------------------------------------

    /// Give every channel one of the driver's render endpoints, keeping
    /// previous assignments so the system default output survives restarts.
    fn assign_slots(&self) {
        let inner = self.inner.borrow();
        let mut cfg = inner.config.borrow_mut();
        let mut used = [false; MAX_SLOTS];
        for ch in &mut cfg.channels {
            match ch.device_slot {
                Some(s) if (s as usize) < MAX_SLOTS && !used[s as usize] => used[s as usize] = true,
                _ => ch.device_slot = None,
            }
        }
        for ch in &mut cfg.channels {
            if ch.device_slot.is_none()
                && let Some(free) = used.iter().position(|u| !u)
            {
                used[free] = true;
                ch.device_slot = Some(free as u8);
            }
        }
    }

    /// Have the naming service call each channel's output after the channel
    /// ("Crossfade: Game"), and free outputs "Crossfade Channel N" again.
    /// Requests only go out when Windows' names differ from the wanted
    /// ones, so this is cheap to call after every change; a request that
    /// did not take effect (no service) is retried at most every 30 s.
    fn sync_endpoint_names(&self) {
        const RETRY: Duration = Duration::from_secs(30);
        let wanted: Vec<(u8, String)> = {
            let inner = self.inner.borrow();
            let cfg = inner.config.borrow();
            (0..MAX_SLOTS as u8)
                .map(|slot| {
                    let name = cfg
                        .channels
                        .iter()
                        .find(|c| {
                            c.device_slot == Some(slot)
                                && c.assignment.as_ref().is_some_and(Assignment::provides_device)
                        })
                        .map(|c| namer::channel_endpoint_name(&c.name))
                        .unwrap_or_default();
                    (slot + 1, name)
                })
                .collect()
        };
        let mismatch = {
            let inner = self.inner.borrow();
            wanted.iter().any(|(slot, name)| {
                let expected = if name.is_empty() {
                    format!("Crossfade Channel {slot}")
                } else {
                    name.clone()
                };
                // Windows shows "<description> (Crossfade Virtual Audio)".
                inner
                    .endpoints
                    .iter()
                    .find(|e| e.role == Some(DriverRole::Channel(slot - 1)))
                    .is_some_and(|e| e.name != expected && !e.name.starts_with(&format!("{expected} (")))
            })
        };
        let mut inner = self.inner.borrow_mut();
        if !mismatch {
            inner.names_requested = None;
            return;
        }
        if let Some((last, at)) = &inner.names_requested
            && *last == wanted
            && at.elapsed() < RETRY
        {
            return;
        }
        inner.names_requested = Some((wanted.clone(), Instant::now()));
        namer::request(wanted);
    }

    fn slot_endpoint(&self, slot: u8) -> Option<String> {
        self.inner
            .borrow()
            .endpoints
            .iter()
            .find(|e| e.role == Some(DriverRole::Channel(slot)))
            .map(|e| e.id.clone())
    }

    fn channel_slot_endpoint(&self, id: u64) -> Option<String> {
        let slot = self.inner.borrow().config.borrow().channel(id)?.device_slot?;
        self.slot_endpoint(slot)
    }

    fn input_spec(&self, id: u64) -> Option<InputSpec> {
        let assignment = self
            .inner
            .borrow()
            .config
            .borrow()
            .channel(id)?
            .assignment
            .clone()?;
        match assignment {
            Assignment::Source { name } => Some(match name.strip_prefix(LOOPBACK_PREFIX) {
                Some(render) => InputSpec::Loopback(render.to_string()),
                None => InputSpec::Capture(name),
            }),
            a if a.provides_device() => self.channel_slot_endpoint(id).map(InputSpec::Loopback),
            _ => None,
        }
    }

    fn send_input(&self, id: u64) {
        let input = self.input_spec(id);
        self.send(Command::SetInput { id, input });
    }

    fn send_gains(&self, id: u64) {
        let gains = {
            let inner = self.inner.borrow();
            let cfg = inner.config.borrow();
            let Some(ch) = cfg.channel(id) else {
                return;
            };
            let g = |mix| {
                let (v, muted) = mix_levels(ch, &cfg.master, mix);
                gain(v, muted)
            };
            [g(Mix::Monitor), g(Mix::Stream), g(Mix::Vod)]
        };
        self.send(Command::SetGains { id, gains });
    }

    /// Recreate the routing for one channel from its config (or tear it
    /// down if the channel was removed).
    pub fn rebuild_channel(&self, id: u64) {
        let exists = self.inner.borrow().config.borrow().channel(id).is_some();
        if !exists {
            self.send(Command::Remove(id));
            self.reconcile_apps();
            self.sync_endpoint_names();
            return;
        }
        let needs_slot = self
            .inner
            .borrow()
            .config
            .borrow()
            .channel(id)
            .is_some_and(|c| c.device_slot.is_none());
        if needs_slot {
            self.assign_slots();
        }
        self.send_input(id);
        self.send_gains(id);
        self.reconcile_apps();
        self.sync_endpoint_names();
    }

    pub fn apply_channel_mix(&self, id: u64, _mix: Mix) {
        self.send_gains(id);
    }

    pub fn apply_master_monitor(&self) {
        let g = {
            let inner = self.inner.borrow();
            let m = &inner.config.borrow().master;
            gain(m.monitor_volume, m.monitor_muted)
        };
        self.send(Command::SetMonitorGain(g));
    }

    fn all_gains(&self) {
        let ids: Vec<u64> = self.inner.borrow().config.borrow().channels.iter().map(|c| c.id).collect();
        for id in ids {
            self.send_gains(id);
        }
    }

    pub fn apply_master_stream(&self) {
        self.all_gains();
    }

    pub fn apply_master_vod(&self) {
        self.all_gains();
    }

    /// The VOD microphone endpoint always exists (driver endpoints are
    /// static); disabling the mix just stops feeding it.
    pub fn apply_vod_mix(&self) {
        let on = self.inner.borrow().config.borrow().vod_mix_enabled;
        self.send(Command::SetVod(on));
    }

    /// Route the monitor mix to the configured output, else the system
    /// default — unless the default is one of our own endpoints, which
    /// would feed the mix back into itself.
    pub fn setup_monitor_output(&self) {
        let target = {
            let inner = self.inner.borrow();
            let configured = inner.config.borrow().master.monitor_device.clone();
            let hardware = |id: &str| {
                inner
                    .endpoints
                    .iter()
                    .any(|e| e.is_render && e.role.is_none() && e.id == id)
            };
            configured
                .filter(|d| hardware(d))
                .or_else(|| inner.default_render.clone().filter(|d| hardware(d)))
        };
        self.send(Command::SetMonitor(target));
    }

    /// Release every per-app pin and stop the engine, then call `done`.
    pub fn shutdown(&self, done: Box<dyn Fn() + 'static>) {
        {
            let mut inner = self.inner.borrow_mut();
            let routes: Vec<u32> = inner.routes.drain().map(|(pid, _)| pid).collect();
            if let Some(routing) = &inner.routing {
                for pid in routes {
                    routing.set(pid, None);
                }
            }
        }
        self.stop_engine();
        done();
    }

    // ---- Per-app routing ----------------------------------------------------------

    fn is_driver_endpoint(&self, id: &str) -> bool {
        self.inner
            .borrow()
            .endpoints
            .iter()
            .any(|e| e.id == id && e.role.is_some())
    }

    /// Pins left behind by a crashed session look exactly like the user's
    /// own choices; treat every existing pin to one of our endpoints as
    /// ours, so the rules below may move or clear it.
    fn adopt_stale_routes(&self) {
        let apps = match self.inner.borrow_mut().sessions.as_mut() {
            Some(s) => s.scan(),
            None => return,
        };
        let pinned: Vec<(u32, String)> = {
            let inner = self.inner.borrow();
            let Some(routing) = &inner.routing else {
                return;
            };
            apps.keys().filter_map(|&pid| routing.get(pid).map(|e| (pid, e))).collect()
        };
        for (pid, endpoint) in pinned {
            if self.is_driver_endpoint(&endpoint) {
                self.inner.borrow_mut().routes.insert(pid, endpoint);
            }
        }
    }

    fn scan_sessions(&self) {
        let apps = match self.inner.borrow_mut().sessions.as_mut() {
            Some(s) => s.scan(),
            None => return,
        };
        let changed = {
            let mut inner = self.inner.borrow_mut();
            let old: BTreeSet<&String> = inner.apps.values().collect();
            let new: BTreeSet<&String> = apps.values().collect();
            let changed = old != new;
            inner.routes.retain(|pid, _| apps.contains_key(pid));
            inner.apps = apps;
            changed
        };
        self.reconcile_apps();
        if changed {
            self.emit(AudioEvent::DevicesChanged);
        }
    }

    /// Pin every playing process to the endpoint of the first App channel
    /// whose rules match it, else to the catch-all channel's. A process
    /// the user pinned to one of our endpoints by hand is left alone.
    fn reconcile_apps(&self) {
        let plan: Vec<(u32, Option<String>)> = {
            let inner = self.inner.borrow();
            let Some(routing) = &inner.routing else {
                return;
            };
            let cfg = inner.config.borrow();
            let catch_all = cfg.catch_all_channel().and_then(|c| c.device_slot);
            let mut plan = Vec::new();
            for (&pid, name) in &inner.apps {
                let wanted_slot = cfg
                    .channels
                    .iter()
                    .find(|c| {
                        matches!(c.assignment, Some(Assignment::App { .. }))
                            && c.assignment
                                .as_ref()
                                .is_some_and(|a| a.apps().iter().any(|r| config::app_matches(r, name)))
                    })
                    .and_then(|c| c.device_slot)
                    .or(catch_all);
                let wanted = wanted_slot.and_then(|slot| {
                    inner
                        .endpoints
                        .iter()
                        .find(|e| e.role == Some(DriverRole::Channel(slot)))
                        .map(|e| e.id.clone())
                });
                let current = routing.get(pid);
                let ours = inner.routes.get(&pid);
                let manual = current.as_ref().is_some_and(|c| {
                    ours != Some(c) && inner.endpoints.iter().any(|e| &e.id == c && e.role.is_some())
                });
                if manual {
                    continue;
                }
                match &wanted {
                    Some(w) if current.as_ref() != Some(w) => plan.push((pid, wanted.clone())),
                    None if ours.is_some() => plan.push((pid, None)),
                    _ => {}
                }
            }
            plan
        };
        if plan.is_empty() {
            return;
        }
        let mut inner = self.inner.borrow_mut();
        for (pid, target) in plan {
            let ok = inner
                .routing
                .as_ref()
                .is_some_and(|r| r.set(pid, target.as_deref()));
            if !ok {
                continue;
            }
            match target {
                Some(t) => {
                    inner.routes.insert(pid, t);
                }
                None => {
                    inner.routes.remove(&pid);
                }
            }
        }
    }

    // ---- Getters for the UI ----------------------------------------------------------

    pub fn sources(&self) -> Vec<SourceEntry> {
        let inner = self.inner.borrow();
        let mut out: Vec<SourceEntry> = inner
            .endpoints
            .iter()
            .filter(|e| e.role.is_none())
            .map(|e| SourceEntry {
                name: if e.is_render {
                    format!("{LOOPBACK_PREFIX}{}", e.id)
                } else {
                    e.id.clone()
                },
                description: if e.is_render {
                    format!("Monitor of {}", e.name)
                } else {
                    e.name.clone()
                },
                is_monitor: e.is_render,
                volume: e.volume,
                channels: e.channels,
                index: 0,
                card: None,
            })
            .collect();
        out.sort_by(|a, b| a.description.cmp(&b.description));
        out
    }

    pub fn output_sinks(&self) -> Vec<SinkEntry> {
        let inner = self.inner.borrow();
        let mut out: Vec<SinkEntry> = inner
            .endpoints
            .iter()
            .filter(|e| e.is_render && e.role.is_none())
            .map(sink_entry)
            .collect();
        out.sort_by(|a, b| a.description.cmp(&b.description));
        out
    }

    pub fn app_names(&self) -> Vec<String> {
        let inner = self.inner.borrow();
        let set: BTreeSet<String> = inner.apps.values().cloned().collect();
        set.into_iter().collect()
    }

    pub fn source_description(&self, name: &str) -> Option<String> {
        self.sources()
            .into_iter()
            .find(|s| s.name == name)
            .map(|s| s.description)
    }

    pub fn default_sink(&self) -> Option<String> {
        self.inner.borrow().default_render.clone()
    }

    pub fn default_source(&self) -> Option<String> {
        self.inner.borrow().default_capture.clone()
    }

    /// Unlike `output_sinks`/`sources`, these also see Crossfade's own devices.
    pub fn has_sink(&self, name: &str) -> bool {
        self.inner
            .borrow()
            .endpoints
            .iter()
            .any(|e| e.is_render && e.id == name)
    }

    pub fn has_source(&self, name: &str) -> bool {
        self.inner
            .borrow()
            .endpoints
            .iter()
            .any(|e| !e.is_render && e.id == name)
    }

    /// Endpoint id of the output device a channel exposes.
    pub fn channel_sink(&self, id: u64) -> Option<String> {
        self.channel_slot_endpoint(id)
    }

    /// How the channel's output endpoint is called in other apps. The
    /// driver's endpoints are fixed, so this is the endpoint's Windows name
    /// (e.g. "Crossfade Channel 3 (Crossfade Virtual Audio)"), not the
    /// channel's.
    pub fn channel_device_label(&self, id: u64) -> String {
        if let Some(name) = self.channel_slot_endpoint(id).and_then(|e| self.device_label(&e)) {
            return name;
        }
        match self.inner.borrow().config.borrow().channel(id).and_then(|c| c.device_slot) {
            Some(slot) => format!("Crossfade Channel {}", slot + 1),
            None => "a Crossfade Channel output".to_string(),
        }
    }

    /// Endpoint id of the "Crossfade Stream Mix" microphone.
    pub fn stream_mic(&self) -> Option<String> {
        self.inner
            .borrow()
            .endpoints
            .iter()
            .find(|e| e.role == Some(DriverRole::StreamMic))
            .map(|e| e.id.clone())
    }

    /// Display name of any endpoint, including our own.
    fn device_label(&self, name: &str) -> Option<String> {
        self.inner
            .borrow()
            .endpoints
            .iter()
            .find(|e| e.id == name)
            .map(|e| e.name.clone())
    }

    pub fn set_default_sink(&self, name: &str) {
        if let Err(e) = policy::set_default_endpoint(name) {
            eprintln!("gleem-crossfade: could not set the default output: {e}");
        }
    }

    pub fn set_default_source(&self, name: &str) {
        if let Err(e) = policy::set_default_endpoint(name) {
            eprintln!("gleem-crossfade: could not set the default input: {e}");
        }
    }

    pub fn set_source_volume(&self, name: &str, percent: f64) {
        if name.starts_with(LOOPBACK_PREFIX) {
            return;
        }
        if let Some(d) = &self.inner.borrow().devices {
            d.set_volume(name, percent);
        }
    }

    pub fn set_sink_volume(&self, name: &str, percent: f64) {
        if let Some(d) = &self.inner.borrow().devices {
            d.set_volume(name, percent);
        }
    }

    /// The Wave XLR's capture endpoint, if connected.
    pub fn wave_xlr_source(&self) -> Option<SourceEntry> {
        self.sources()
            .into_iter()
            .find(|s| !s.is_monitor && is_wave_xlr(&s.name, &s.description))
    }

    /// The Wave XLR's output endpoint, if connected.
    pub fn wave_xlr_sink(&self) -> Option<SinkEntry> {
        self.output_sinks()
            .into_iter()
            .find(|s| is_wave_xlr(&s.name, &s.description))
    }

    /// The capture endpoint of any connected Elgato Wave interface.
    pub fn wave_source(&self) -> Option<SourceEntry> {
        self.sources()
            .into_iter()
            .find(|s| !s.is_monitor && is_elgato_wave(&s.name, &s.description))
    }

    /// The output endpoint of any connected Elgato Wave interface.
    pub fn wave_sink(&self) -> Option<SinkEntry> {
        self.output_sinks()
            .into_iter()
            .find(|s| is_elgato_wave(&s.name, &s.description))
    }
}

fn sink_entry(e: &Endpoint) -> SinkEntry {
    SinkEntry {
        name: e.id.clone(),
        description: e.name.clone(),
        volume: e.volume,
        channels: e.channels,
        card: None,
    }
}
