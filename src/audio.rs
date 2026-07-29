//! PipeWire audio routing via the PulseAudio client API (served by
//! `pipewire-pulse`), running entirely on the GTK main loop.
//!
//! Topology created on the server:
//!
//! ```text
//!  capture source ──┬── module-loopback ──▶ Crossfade_Monitor ── loopback ──▶ headphones
//!  (or app stream    ├── module-loopback ──▶ Crossfade_Stream  (captured by OBS/Discord)
//!   moved into a     └── module-loopback ──▶ Crossfade_Vod     (optional third bus for a
//!   per-channel                                                DMCA-safe VOD/recording track)
//!   null sink)
//! ```
//!
//! Every loopback shows up as a sink-input owned by the module we loaded, so
//! per-channel/per-mix volume and mute map to plain sink-input operations.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use gtk::glib;
use libpulse_binding as pulse;
use libpulse_glib_binding as pulse_glib;

use pulse::callbacks::ListResult;
use pulse::context::subscribe::{Facility, InterestMaskSet};
use pulse::context::{Context, FlagSet as CtxFlagSet, State as CtxState};
use pulse::def::BufferAttr;
use pulse::proplist::{properties, Proplist};
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlagSet, PeekResult, Stream};
use pulse::volume::{ChannelVolumes, Volume};

use crate::config::{Assignment, ChannelConfig, Config, MasterConfig};
use crate::fx::{self, FxEvent, FxManager};
use crate::lv2;

pub const MONITOR_SINK: &str = "Crossfade_Monitor";
pub const STREAM_SINK: &str = "Crossfade_Stream";
pub const STREAM_MIC: &str = "Crossfade_StreamMic";
pub const VOD_SINK: &str = "Crossfade_Vod";
pub const VOD_MIC: &str = "Crossfade_VodMic";
const OWN_PREFIX: &str = "Crossfade_";
/// Pre-rename (OpenWave) server prefix, still reaped by the startup cleanup
/// so leftovers from a crashed pre-0.8 session can't linger. Remove after a
/// transition release or two.
const LEGACY_PREFIX: &str = "OpenWave_";
const LOOPBACK_ARGS: &str = "latency_msec=30";
const INVALID_INDEX: u32 = u32::MAX;

pub fn channel_sink_name(id: u64) -> String {
    format!("{OWN_PREFIX}Ch{id}")
}

/// Does this device name/description belong to an Elgato Wave XLR?
fn is_wave_xlr(name: &str, description: &str) -> bool {
    let n = name.to_ascii_lowercase();
    description.contains("Wave XLR") || n.contains("wave_xlr") || n.contains("wave-xlr")
}

/// Strip characters that would break PulseAudio module argument quoting.
fn sanitize_desc(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '"' | '\'' | '\\'))
        .collect()
}

/// Loopback module arguments. Every stream gets a unique, stable
/// `media.name` and opts out of session-manager volume/target restoring —
/// otherwise WirePlumber "restores" our loopbacks and meters onto whatever
/// sink/source a same-named stream used in an earlier session.
fn loopback_args(source: &str, sink: &str, tag: &str) -> String {
    format!(
        "source=\"{source}\" sink=\"{sink}\" {LOOPBACK_ARGS} \
         sink_input_properties='media.name=\"{tag} out\" state.restore-props=false state.restore-target=false' \
         source_output_properties='media.name=\"{tag} in\" state.restore-props=false state.restore-target=false'"
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mix {
    Monitor,
    Stream,
    /// Optional third bus for a VOD-safe recording track.
    Vod,
}

impl Mix {
    pub fn label(&self) -> &'static str {
        match self {
            Mix::Monitor => "Monitor",
            Mix::Stream => "Stream",
            Mix::Vod => "VOD",
        }
    }

    /// Parse the serde/D-Bus name ("monitor" | "stream" | "vod").
    pub fn from_key(key: &str) -> Option<Mix> {
        match key {
            "monitor" => Some(Mix::Monitor),
            "stream" => Some(Mix::Stream),
            "vod" => Some(Mix::Vod),
            _ => None,
        }
    }
}

/// Addresses one managed loopback, for the fast post-load volume apply.
#[derive(Clone, Copy)]
enum LoopbackRef {
    Mix(u64, Mix),
    Input(u64),
    MonitorOut,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LevelTarget {
    Channel(u64),
    MonitorMix,
    StreamMix,
    VodMix,
}

#[derive(Clone, Debug)]
pub enum AudioEvent {
    /// Connected and both virtual mix sinks exist.
    Ready,
    Failed(String),
    DevicesChanged,
    /// Peak levels per channel (one entry for mono, two for stereo).
    Level(LevelTarget, Vec<f64>),
    /// A channel's VST rack finished (re)loading; parameter info changed.
    VstChanged(u64),
    /// VST parameters were edited from a plugin's own window and written
    /// into the config: (channel, [(plugin cfg_id, param index, value)]).
    /// The config should be persisted and open dialog sliders updated.
    VstParams(u64, Vec<(u64, u32, f64)>),
}

#[derive(Clone, Debug)]
pub struct SourceEntry {
    pub name: String,
    pub description: String,
    pub is_monitor: bool,
    /// Current device volume in percent (avg across channels).
    pub volume: f64,
    pub channels: u8,
    /// Server-side index, for matching capture streams to their device.
    pub index: u32,
    /// Owning sound card, for same-card checks against sinks.
    pub card: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct SinkEntry {
    pub name: String,
    pub description: String,
    /// Current device volume in percent (avg across channels).
    pub volume: f64,
    pub channels: u8,
    /// Owning sound card, for same-card checks against sources.
    pub card: Option<u32>,
}

#[derive(Clone, Debug)]
struct SinkInputEntry {
    index: u32,
    app_name: Option<String>,
    sink: u32,
    owner_module: Option<u32>,
    channels: u8,
    volume_raw: u32,
    mute: bool,
}

#[derive(Clone, Debug)]
struct SourceOutputEntry {
    source: u32,
    owner_module: Option<u32>,
}

/// Stream re-attachments queued by a drift check.
#[derive(Default)]
struct DriftMoves {
    /// (sink-input, sink name)
    sinks: Vec<(u32, String)>,
    /// (source-output, source name)
    sources: Vec<(u32, String)>,
}

#[derive(Default)]
struct Loopback {
    module: Option<u32>,
    sink_input: Option<u32>,
    /// The loopback's capture stream, tracked so it can be re-attached when
    /// it ends up on the wrong device (or unlinked because the device was
    /// absent when the stream was created).
    source_output: Option<u32>,
    channels: u8,
    /// Sink this loopback must stay attached to (empty = not enforced yet).
    target: String,
    /// Source the capture side must stay attached to (empty = not enforced).
    source_target: String,
}

/// Wiring deferred until the channel's FX bridge nodes show up on the
/// server (the filter-chain runs in a child process and appears
/// asynchronously).
struct PendingWire {
    /// The channel's real input (capture source or `<sink>.monitor`).
    source: String,
    /// Route through the VST rack (VSTIn sink → pw-links) first.
    use_vst: bool,
}

#[derive(Default)]
struct ChannelRuntime {
    /// Bumped on every rebuild so stale module-load callbacks can tell they
    /// lost the race and must unload what they just created.
    generation: u64,
    sink_module: Option<u32>,
    /// Feeds the input into the FX bridge when effects are active.
    input_loop: Loopback,
    /// Null sink in front of the Carla rack (JACK clients are not valid
    /// loopback targets, so the rack taps this sink's monitor via pw-link).
    vstin_module: Option<u32>,
    pending_wire: Option<PendingWire>,
    /// Configured capture source that is not on the server yet; the channel
    /// is wired as soon as it appears (USB devices enumerate late at login).
    pending_source: Option<String>,
    monitor_loop: Loopback,
    stream_loop: Loopback,
    vod_loop: Loopback,
}

impl ChannelRuntime {
    fn mix_loop(&self, mix: Mix) -> &Loopback {
        match mix {
            Mix::Monitor => &self.monitor_loop,
            Mix::Stream => &self.stream_loop,
            Mix::Vod => &self.vod_loop,
        }
    }

    fn mix_loop_mut(&mut self, mix: Mix) -> &mut Loopback {
        match mix {
            Mix::Monitor => &mut self.monitor_loop,
            Mix::Stream => &mut self.stream_loop,
            Mix::Vod => &mut self.vod_loop,
        }
    }
}

/// Effective volume and mute of one channel's send into a mix (the stream
/// and VOD sends fold their master level in, since those buses have no
/// master loopback of their own).
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

#[derive(Default)]
struct Inner {
    mainloop: Option<pulse_glib::Mainloop>,
    context: Option<Context>,
    ready: bool,
    shutting_down: bool,
    handler: Option<Rc<dyn Fn(AudioEvent)>>,
    config: Rc<RefCell<Config>>,
    owned_modules: HashSet<u32>,
    sinks: Vec<(u32, SinkEntry)>,
    sources: Vec<SourceEntry>,
    sink_inputs: HashMap<u32, SinkInputEntry>,
    source_outputs: HashMap<u32, SourceOutputEntry>,
    default_sink: Option<String>,
    default_source: Option<String>,
    /// Per-channel runtime state, keyed by the channel's stable config id.
    channels: HashMap<u64, ChannelRuntime>,
    /// Modules making up the optional VOD bus (null sink + remap-source),
    /// tracked separately so disabling the mix can unload exactly these.
    vod_modules: Vec<u32>,
    /// Effect helper processes (filter chains, Carla racks).
    fx: FxManager,
    monitor_out: Loopback,
    monitor_out_generation: u64,
    monitor_out_pending: bool,
    /// Suppresses monitor-out creation while capture wiring is settling.
    /// The Wave XLR's firmware delivers a silent mic when its playback
    /// stream is opened before its capture stream, so capture must come up
    /// first; see check_pending_sources / source_channels_settled.
    monitor_out_deferred: bool,
    peaks: HashMap<LevelTarget, Rc<RefCell<Stream>>>,
    refresh_queued: bool,
}

#[derive(Clone)]
pub struct PulseManager {
    inner: Rc<RefCell<Inner>>,
}

fn volume_cv(channels: u8, v: f64) -> ChannelVolumes {
    let raw = (f64::from(Volume::NORMAL.0) * v.clamp(0.0, 1.0)).round() as u32;
    let mut cv = ChannelVolumes::default();
    cv.set(channels.max(1), Volume(raw));
    cv
}

impl PulseManager {
    pub fn new(config: Rc<RefCell<Config>>) -> Self {
        let inner = Inner {
            config,
            ..Inner::default()
        };
        let rc = Rc::new(RefCell::new(inner));
        let weak = Rc::downgrade(&rc);
        rc.borrow_mut().fx.set_event_handler(move |ev| {
            let Some(rc) = weak.upgrade() else {
                return;
            };
            // Child-watch/pipe context: never called while Inner is borrowed.
            match ev {
                FxEvent::ChainDied(id) | FxEvent::VstHostDied(id) => {
                    if rc.borrow().shutting_down {
                        return;
                    }
                    // Rewire the channel around whatever is still running
                    // (chain gone → direct, VST host gone → chain only).
                    Self::rebuild_channel_inner(&rc, id);
                    Self::emit(&rc, AudioEvent::DevicesChanged);
                }
                FxEvent::VstReply(id, line) => {
                    let outcome = rc.borrow_mut().fx.handle_vst_reply(id, &line);
                    if !outcome.params.is_empty() {
                        // Edits made in a plugin's native window: persist
                        // them like slider changes.
                        let inner = rc.borrow();
                        let mut cfg = inner.config.borrow_mut();
                        if let Some(ch) = cfg.channel_mut(id) {
                            for (cfg_id, index, value) in &outcome.params {
                                if let Some(p) = ch.vst_mut(*cfg_id) {
                                    p.params.insert(index.to_string(), *value);
                                }
                            }
                        }
                        drop(cfg);
                        drop(inner);
                        Self::emit(&rc, AudioEvent::VstParams(id, outcome.params));
                    }
                    if outcome.structure_changed {
                        Self::emit(&rc, AudioEvent::VstChanged(id));
                    }
                }
            }
        });
        Self { inner: rc }
    }

    pub fn set_event_handler(&self, f: impl Fn(AudioEvent) + 'static) {
        self.inner.borrow_mut().handler = Some(Rc::new(f));
    }

    /// (Re)connect to the sound server, resetting all runtime state.
    pub fn connect_server(&self) {
        let rc = &self.inner;
        {
            let mut inner = rc.borrow_mut();
            inner.ready = false;
            inner.shutting_down = false;
            inner.owned_modules.clear();
            inner.channels.clear();
            inner.vod_modules.clear();
            inner.fx.shutdown_all();
            inner.monitor_out = Loopback::default();
            inner.monitor_out_pending = false;
            inner.monitor_out_deferred = false;
            for (_, s) in inner.peaks.drain() {
                Self::drop_peak(&s);
            }
            inner.sinks.clear();
            inner.sources.clear();
            inner.sink_inputs.clear();
            inner.source_outputs.clear();
            inner.refresh_queued = false;
            inner.context = None;
            inner.mainloop = None;
        }

        let Some(ml) = pulse_glib::Mainloop::new(None) else {
            Self::fail(rc, "Could not create the audio main loop");
            return;
        };
        let mut proplist = Proplist::new().expect("proplist");
        let _ = proplist.set_str(properties::APPLICATION_NAME, "Gleem Crossfade");
        let _ = proplist.set_str(properties::APPLICATION_ID, crate::APP_ID);
        let _ = proplist.set_str(properties::APPLICATION_ICON_NAME, "audio-card");
        let Some(mut ctx) = Context::new_with_proplist(&ml, "Gleem Crossfade", &proplist) else {
            Self::fail(rc, "Could not create the audio server context");
            return;
        };
        let weak = Rc::downgrade(rc);
        ctx.set_state_callback(Some(Box::new(move || {
            if let Some(rc) = weak.upgrade() {
                Self::on_state_change(&rc);
            }
        })));
        if let Err(e) = ctx.connect(None, CtxFlagSet::NOFLAGS, None) {
            Self::fail(rc, &format!("Could not connect to the audio server: {e}"));
            return;
        }
        let mut inner = rc.borrow_mut();
        inner.mainloop = Some(ml);
        inner.context = Some(ctx);
    }

    // ---- Public reconcilers driven by the UI --------------------------------

    /// Tear down and recreate the routing for one channel from its config.
    /// If the channel no longer exists in the config, this just tears it down.
    pub fn rebuild_channel(&self, id: u64) {
        Self::rebuild_channel_inner(&self.inner, id);
    }

    pub fn apply_channel_mix(&self, id: u64, mix: Mix) {
        Self::apply_channel_mix_inner(&self.inner, id, mix);
    }

    pub fn apply_master_monitor(&self) {
        Self::apply_master_monitor_inner(&self.inner);
    }

    pub fn apply_master_stream(&self) {
        for id in self.channel_ids() {
            Self::apply_channel_mix_inner(&self.inner, id, Mix::Stream);
        }
    }

    pub fn apply_master_vod(&self) {
        for id in self.channel_ids() {
            Self::apply_channel_mix_inner(&self.inner, id, Mix::Vod);
        }
    }

    fn channel_ids(&self) -> Vec<u64> {
        let inner = self.inner.borrow();
        let cfg = inner.config.borrow();
        cfg.channels.iter().map(|c| c.id).collect()
    }

    /// Bring the server state in line with `config.vod_mix_enabled`: create
    /// or tear down the VOD bus, its "Crossfade VOD Mix" capture device, its
    /// master meter and every channel's send into it — without touching the
    /// monitor/stream wiring, so toggling the preference never interrupts
    /// audio.
    pub fn apply_vod_mix(&self) {
        Self::apply_vod_mix_inner(&self.inner);
    }

    /// Route the monitor mix to the configured (or default) hardware sink.
    pub fn setup_monitor_output(&self) {
        Self::setup_monitor_output_inner(&self.inner);
    }

    /// Push a control-port value into a channel's running filter chain
    /// without rebuilding it. `mono` mirrors the value onto both per-lane
    /// instances of a mono plugin.
    pub fn set_effect_control(
        &self,
        channel: u64,
        effect_id: u64,
        mono: bool,
        symbol: &str,
        value: f64,
    ) {
        let inner = self.inner.borrow();
        inner
            .fx
            .set_controls(channel, &fx::effect_labels(effect_id, mono), symbol, value);
    }

    /// Runtime state (load status + parameters) of a channel's VST rack.
    pub fn vst_runtime(&self, id: u64) -> Vec<fx::VstRuntime> {
        self.inner.borrow().fx.vst_runtime(id)
    }

    /// Push a parameter value into a channel's running VST rack without
    /// rebuilding anything.
    pub fn set_vst_param(&self, channel: u64, cfg_id: u64, index: u32, value: f64) {
        self.inner
            .borrow_mut()
            .fx
            .set_vst_param(channel, cfg_id, index, value);
    }

    /// Open a VST plugin's own editor window (hosted by the helper).
    pub fn show_vst_ui(&self, channel: u64, cfg_id: u64) {
        self.inner.borrow_mut().fx.show_vst_ui(channel, cfg_id, true);
    }

    /// Unload everything we created on the server, then call `done`.
    pub fn shutdown(&self, done: Box<dyn Fn() + 'static>) {
        let rc = &self.inner;
        let mods: Vec<u32> = {
            let mut inner = rc.borrow_mut();
            inner.shutting_down = true;
            inner.ready = false;
            inner.fx.shutdown_all();
            for (_, s) in inner.peaks.drain() {
                Self::drop_peak(&s);
            }
            inner.owned_modules.drain().collect()
        };
        let Some(mut intro) = Self::introspect(rc) else {
            done();
            return;
        };
        if mods.is_empty() {
            done();
            return;
        }
        let remaining = Rc::new(Cell::new(mods.len()));
        let done: Rc<dyn Fn()> = Rc::from(done);
        for m in mods {
            let remaining = remaining.clone();
            let done = done.clone();
            let _ = intro.unload_module(m, move |_| {
                remaining.set(remaining.get().saturating_sub(1));
                if remaining.get() == 0 {
                    done();
                }
            });
        }
    }

    // ---- Getters for the UI --------------------------------------------------

    pub fn sources(&self) -> Vec<SourceEntry> {
        self.inner
            .borrow()
            .sources
            .iter()
            .filter(|s| !s.name.starts_with(OWN_PREFIX))
            .cloned()
            .collect()
    }

    pub fn output_sinks(&self) -> Vec<SinkEntry> {
        self.inner
            .borrow()
            .sinks
            .iter()
            .filter(|(_, s)| !s.name.starts_with(OWN_PREFIX))
            .map(|(_, s)| s.clone())
            .collect()
    }

    pub fn app_names(&self) -> Vec<String> {
        let inner = self.inner.borrow();
        let mut set = BTreeSet::new();
        for si in inner.sink_inputs.values() {
            if si
                .owner_module
                .is_some_and(|m| inner.owned_modules.contains(&m))
            {
                continue;
            }
            if let Some(n) = &si.app_name
                && !n.is_empty() {
                    set.insert(n.clone());
                }
        }
        set.into_iter().collect()
    }

    pub fn source_description(&self, name: &str) -> Option<String> {
        self.inner
            .borrow()
            .sources
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.description.clone())
    }

    pub fn default_sink(&self) -> Option<String> {
        self.inner.borrow().default_sink.clone()
    }

    pub fn default_source(&self) -> Option<String> {
        self.inner.borrow().default_source.clone()
    }

    /// Unlike `output_sinks`/`sources`, these also see Crossfade's own devices.
    pub fn has_sink(&self, name: &str) -> bool {
        self.inner.borrow().sinks.iter().any(|(_, e)| e.name == name)
    }

    pub fn has_source(&self, name: &str) -> bool {
        self.inner.borrow().sources.iter().any(|s| s.name == name)
    }

    pub fn set_default_sink(&self, name: &str) {
        let mut inner = self.inner.borrow_mut();
        if let Some(ctx) = inner
            .context
            .as_mut()
            .filter(|c| c.get_state() == CtxState::Ready)
        {
            let _ = ctx.set_default_sink(name, |_| {});
        }
    }

    pub fn set_default_source(&self, name: &str) {
        let mut inner = self.inner.borrow_mut();
        if let Some(ctx) = inner
            .context
            .as_mut()
            .filter(|c| c.get_state() == CtxState::Ready)
        {
            let _ = ctx.set_default_source(name, |_| {});
        }
    }

    /// Set a capture device's volume (percent of `Volume::NORMAL`).
    pub fn set_source_volume(&self, name: &str, percent: f64) {
        let channels = {
            let inner = self.inner.borrow();
            let Some(s) = inner.sources.iter().find(|s| s.name == name) else {
                return;
            };
            s.channels
        };
        let Some(mut intro) = Self::introspect(&self.inner) else {
            return;
        };
        let cv = volume_cv(channels, percent / 100.0);
        let _ = intro.set_source_volume_by_name(name, &cv, None);
    }

    /// Set an output device's volume (percent of `Volume::NORMAL`).
    pub fn set_sink_volume(&self, name: &str, percent: f64) {
        let channels = {
            let inner = self.inner.borrow();
            let Some((_, s)) = inner.sinks.iter().find(|(_, e)| e.name == name) else {
                return;
            };
            s.channels
        };
        let Some(mut intro) = Self::introspect(&self.inner) else {
            return;
        };
        let cv = volume_cv(channels, percent / 100.0);
        let _ = intro.set_sink_volume_by_name(name, &cv, None);
    }

    /// The Wave XLR's capture device ("Elgato Wave XLR Mono"), if connected.
    pub fn wave_xlr_source(&self) -> Option<SourceEntry> {
        let inner = self.inner.borrow();
        let mut matches = inner.sources.iter().filter(|s| {
            !s.is_monitor
                && !s.name.starts_with(OWN_PREFIX)
                && is_wave_xlr(&s.name, &s.description)
        });
        matches.next().cloned()
    }

    /// The Wave XLR's output device ("Elgato Wave XLR Analog Stereo"), if
    /// connected.
    pub fn wave_xlr_sink(&self) -> Option<SinkEntry> {
        let inner = self.inner.borrow();
        inner
            .sinks
            .iter()
            .map(|(_, e)| e)
            .find(|e| !e.name.starts_with(OWN_PREFIX) && is_wave_xlr(&e.name, &e.description))
            .cloned()
    }

    // ---- Internals -----------------------------------------------------------

    fn emit(rc: &Rc<RefCell<Inner>>, ev: AudioEvent) {
        let handler = rc.borrow().handler.clone();
        if let Some(h) = handler {
            glib::idle_add_local_once(move || h(ev));
        }
    }

    fn fail(rc: &Rc<RefCell<Inner>>, msg: &str) {
        rc.borrow_mut().ready = false;
        Self::emit(rc, AudioEvent::Failed(msg.to_string()));
    }

    /// Detach a peak stream so it can be dropped safely. The read callback
    /// must be cleared first: libpulse may still hold queued events for the
    /// stream, and dispatching one into the freed closure after the Rust
    /// wrapper is gone crashes with a wild jump (observed as SIGSEGV in
    /// do_read/request_cb_proxy when a metered source vanishes mid-rebuild).
    fn drop_peak(stream: &Rc<RefCell<Stream>>) {
        let mut s = stream.borrow_mut();
        s.set_read_callback(None);
        let _ = s.disconnect();
    }

    fn introspect(
        rc: &Rc<RefCell<Inner>>,
    ) -> Option<pulse::context::introspect::Introspector> {
        let inner = rc.borrow();
        inner
            .context
            .as_ref()
            .filter(|c| c.get_state() == CtxState::Ready)
            .map(|c| c.introspect())
    }

    fn on_state_change(rc: &Rc<RefCell<Inner>>) {
        let state = rc.borrow().context.as_ref().map(|c| c.get_state());
        match state {
            Some(CtxState::Ready) => Self::on_ready(rc),
            Some(CtxState::Failed) => {
                Self::fail(rc, "The connection to the audio server failed.")
            }
            Some(CtxState::Terminated)
                if !rc.borrow().shutting_down => {
                    Self::fail(rc, "The audio server terminated the connection.");
                }
            _ => {}
        }
    }

    fn on_ready(rc: &Rc<RefCell<Inner>>) {
        {
            let mut inner = rc.borrow_mut();
            let weak = Rc::downgrade(rc);
            let Some(ctx) = inner.context.as_mut() else {
                return;
            };
            ctx.set_subscribe_callback(Some(Box::new(move |facility, _op, _idx| {
                if let Some(rc) = weak.upgrade()
                    && matches!(
                        facility,
                        Some(
                            Facility::Sink
                                | Facility::Source
                                | Facility::SinkInput
                                | Facility::SourceOutput
                                | Facility::Server
                        )
                    ) {
                        Self::schedule_refresh(&rc);
                    }
            })));
            ctx.subscribe(
                InterestMaskSet::SINK
                    | InterestMaskSet::SOURCE
                    | InterestMaskSet::SINK_INPUT
                    | InterestMaskSet::SOURCE_OUTPUT
                    | InterestMaskSet::SERVER,
                |_| {},
            );
        }
        Self::cleanup_leftovers(rc);
    }

    /// Unload Crossfade modules left over from a crashed session, then create
    /// the virtual sinks fresh.
    fn cleanup_leftovers(rc: &Rc<RefCell<Inner>>) {
        let Some(intro) = Self::introspect(rc) else {
            return;
        };
        let weak = Rc::downgrade(rc);
        let found: Rc<RefCell<Vec<u32>>> = Rc::default();
        let _ = intro.get_module_info_list(move |res| match res {
            ListResult::Item(m) => {
                let name = m.name.as_deref().unwrap_or("");
                let arg = m.argument.as_deref().unwrap_or("");
                if (name == "module-null-sink"
                    || name == "module-loopback"
                    || name == "module-remap-source")
                    && (arg.contains(OWN_PREFIX) || arg.contains(LEGACY_PREFIX))
                {
                    found.borrow_mut().push(m.index);
                }
            }
            ListResult::End | ListResult::Error => {
                let Some(rc) = weak.upgrade() else {
                    return;
                };
                let mods = found.take();
                let Some(mut intro) = Self::introspect(&rc) else {
                    return;
                };
                if mods.is_empty() {
                    Self::create_virtual_sinks(&rc);
                    return;
                }
                // Wait for every unload to finish before creating the new
                // buses: a leftover sink with the same name would win the
                // race, and our meters/loopbacks (which attach by name)
                // would bind to the doomed node and die with it.
                let remaining = Rc::new(Cell::new(mods.len()));
                for m in mods {
                    let weak = weak.clone();
                    let remaining = remaining.clone();
                    let _ = intro.unload_module(m, move |_| {
                        remaining.set(remaining.get().saturating_sub(1));
                        if remaining.get() == 0
                            && let Some(rc) = weak.upgrade() {
                                Self::create_virtual_sinks(&rc);
                            }
                    });
                }
            }
        });
    }

    fn create_virtual_sinks(rc: &Rc<RefCell<Inner>>) {
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let vod = rc.borrow().config.borrow().vod_mix_enabled;
        // The buses deliberately are NOT called "Virtual … Mix": they show up
        // in speaker lists, and the name users should look for — the
        // "Crossfade Stream Mix" microphone — belongs to the remap-source
        // created in `create_stream_mic`.
        let mut buses = vec![
            (MONITOR_SINK, "Crossfade Monitor Bus", "audio-headphones"),
            (STREAM_SINK, "Crossfade Stream Bus", "audio-input-microphone"),
        ];
        if vod {
            buses.push((VOD_SINK, "Crossfade VOD Bus", "audio-input-microphone"));
        }
        let pending = Rc::new(Cell::new(buses.len()));
        for (name, desc, icon) in buses {
            let args = format!(
                "sink_name={name} sink_properties='device.description=\"{desc}\" device.icon_name={icon}'"
            );
            let weak = Rc::downgrade(rc);
            let pending = pending.clone();
            let _ = intro.load_module("module-null-sink", &args, move |idx| {
                let Some(rc) = weak.upgrade() else {
                    return;
                };
                if idx == INVALID_INDEX {
                    Self::fail(&rc, "Could not create the virtual mix devices.");
                    return;
                }
                {
                    let mut inner = rc.borrow_mut();
                    inner.owned_modules.insert(idx);
                    if name == VOD_SINK {
                        inner.vod_modules.push(idx);
                    }
                }
                pending.set(pending.get().saturating_sub(1));
                if pending.get() == 0 {
                    Self::finish_bootstrap(&rc);
                }
            });
        }
    }

    fn finish_bootstrap(rc: &Rc<RefCell<Inner>>) {
        {
            let mut inner = rc.borrow_mut();
            if inner.shutting_down {
                return;
            }
            inner.ready = true;
            // Hold back the monitor-out playback until the capture loopbacks
            // are up (capture-before-playback; see monitor_out_deferred).
            inner.monitor_out_deferred = true;
        }
        Self::monitor_out_defer_timeout(rc);
        Self::emit(rc, AudioEvent::Ready);
        let vod = rc.borrow().config.borrow().vod_mix_enabled;
        Self::create_mix_mic(rc, STREAM_SINK, STREAM_MIC, "Crossfade Stream Mix");
        if vod {
            Self::create_mix_mic(rc, VOD_SINK, VOD_MIC, "Crossfade VOD Mix");
        }
        let ids: Vec<u64> = {
            let inner = rc.borrow();
            let cfg = inner.config.borrow();
            cfg.channels.iter().map(|c| c.id).collect()
        };
        for id in ids {
            Self::rebuild_channel_inner(rc, id);
        }
        Self::create_peak(rc, LevelTarget::MonitorMix, &format!("{MONITOR_SINK}.monitor"));
        Self::create_peak(rc, LevelTarget::StreamMix, &format!("{STREAM_SINK}.monitor"));
        if vod {
            Self::create_peak(rc, LevelTarget::VodMix, &format!("{VOD_SINK}.monitor"));
        }
        Self::schedule_refresh(rc);
    }

    /// Expose a mix bus as a real capture device ("Crossfade Stream Mix" /
    /// "Crossfade VOD Mix") so applications that hide monitor sources —
    /// Discord, most WebRTC apps — can select it as their microphone.
    fn create_mix_mic(rc: &Rc<RefCell<Inner>>, bus: &str, source_name: &str, desc: &str) {
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let args = format!(
            "master={bus}.monitor source_name={source_name} \
             source_properties='device.description=\"{desc}\" device.icon_name=audio-input-microphone'"
        );
        let weak = Rc::downgrade(rc);
        let vod = source_name == VOD_MIC;
        let _ = intro.load_module("module-remap-source", &args, move |idx| {
            let Some(rc) = weak.upgrade() else {
                return;
            };
            if idx == INVALID_INDEX {
                return;
            }
            let mut inner = rc.borrow_mut();
            inner.owned_modules.insert(idx);
            if vod {
                inner.vod_modules.push(idx);
            }
        });
    }

    /// Safety valve for `monitor_out_deferred`: never leave the user without
    /// monitor audio when capture wiring stalls (device absent, slow FX
    /// spawn, LV2 warm-up). A stale timeout can at worst clear a later
    /// deferral early, which merely restores today's playback-first order.
    fn monitor_out_defer_timeout(rc: &Rc<RefCell<Inner>>) {
        let weak = Rc::downgrade(rc);
        glib::timeout_add_local_once(std::time::Duration::from_secs(8), move || {
            let Some(rc) = weak.upgrade() else {
                return;
            };
            let expired = {
                let mut inner = rc.borrow_mut();
                std::mem::replace(&mut inner.monitor_out_deferred, false)
            };
            if expired {
                Self::schedule_refresh(&rc);
            }
        });
    }

    fn schedule_refresh(rc: &Rc<RefCell<Inner>>) {
        {
            let mut inner = rc.borrow_mut();
            if inner.refresh_queued || inner.shutting_down {
                return;
            }
            inner.refresh_queued = true;
        }
        let weak = Rc::downgrade(rc);
        glib::idle_add_local_once(move || {
            if let Some(rc) = weak.upgrade() {
                rc.borrow_mut().refresh_queued = false;
                Self::refresh(&rc);
            }
        });
    }

    fn refresh(rc: &Rc<RefCell<Inner>>) {
        let Some(intro) = Self::introspect(rc) else {
            return;
        };
        {
            let weak = Rc::downgrade(rc);
            let _ = intro.get_server_info(move |info| {
                if let Some(rc) = weak.upgrade() {
                    let mut inner = rc.borrow_mut();
                    inner.default_sink =
                        info.default_sink_name.as_ref().map(|c| c.to_string());
                    inner.default_source =
                        info.default_source_name.as_ref().map(|c| c.to_string());
                }
            });
        }
        let weak = Rc::downgrade(rc);
        let acc: Rc<RefCell<Vec<(u32, SinkEntry)>>> = Rc::default();
        let _ = intro.get_sink_info_list(move |res| match res {
            ListResult::Item(s) => {
                if let Some(name) = s.name.as_ref() {
                    acc.borrow_mut().push((
                        s.index,
                        SinkEntry {
                            name: name.to_string(),
                            description: s
                                .description
                                .as_ref()
                                .map(|d| d.to_string())
                                .unwrap_or_else(|| name.to_string()),
                            volume: 100.0 * f64::from(s.volume.avg().0)
                                / f64::from(Volume::NORMAL.0),
                            channels: s.volume.len(),
                            card: s.card,
                        },
                    ));
                }
            }
            ListResult::End | ListResult::Error => {
                let Some(rc) = weak.upgrade() else {
                    return;
                };
                rc.borrow_mut().sinks = acc.take();
                Self::refresh_sources(&rc);
            }
        });
    }

    fn refresh_sources(rc: &Rc<RefCell<Inner>>) {
        let Some(intro) = Self::introspect(rc) else {
            return;
        };
        let weak = Rc::downgrade(rc);
        let acc: Rc<RefCell<Vec<SourceEntry>>> = Rc::default();
        let _ = intro.get_source_info_list(move |res| match res {
            ListResult::Item(s) => {
                if let Some(name) = s.name.as_ref() {
                    let name = name.to_string();
                    acc.borrow_mut().push(SourceEntry {
                        is_monitor: name.ends_with(".monitor"),
                        description: s
                            .description
                            .as_ref()
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| name.clone()),
                        volume: 100.0 * f64::from(s.volume.avg().0)
                            / f64::from(Volume::NORMAL.0),
                        channels: s.volume.len(),
                        index: s.index,
                        card: s.card,
                        name,
                    });
                }
            }
            ListResult::End | ListResult::Error => {
                let Some(rc) = weak.upgrade() else {
                    return;
                };
                rc.borrow_mut().sources = acc.take();
                Self::refresh_sink_inputs(&rc);
            }
        });
    }

    fn refresh_sink_inputs(rc: &Rc<RefCell<Inner>>) {
        let Some(intro) = Self::introspect(rc) else {
            return;
        };
        let weak = Rc::downgrade(rc);
        let acc: Rc<RefCell<HashMap<u32, SinkInputEntry>>> = Rc::default();
        let _ = intro.get_sink_input_info_list(move |res| match res {
            ListResult::Item(si) => {
                let app_name = si
                    .proplist
                    .get_str("application.name")
                    .or_else(|| si.name.as_ref().map(|c| c.to_string()));
                acc.borrow_mut().insert(
                    si.index,
                    SinkInputEntry {
                        index: si.index,
                        app_name,
                        sink: si.sink,
                        owner_module: si.owner_module,
                        channels: si.volume.len(),
                        volume_raw: si.volume.avg().0,
                        mute: si.mute,
                    },
                );
            }
            ListResult::End | ListResult::Error => {
                let Some(rc) = weak.upgrade() else {
                    return;
                };
                rc.borrow_mut().sink_inputs = acc.take();
                Self::refresh_source_outputs(&rc);
            }
        });
    }

    fn refresh_source_outputs(rc: &Rc<RefCell<Inner>>) {
        let Some(intro) = Self::introspect(rc) else {
            return;
        };
        let weak = Rc::downgrade(rc);
        let acc: Rc<RefCell<HashMap<u32, SourceOutputEntry>>> = Rc::default();
        let _ = intro.get_source_output_info_list(move |res| match res {
            ListResult::Item(so) => {
                acc.borrow_mut().insert(
                    so.index,
                    SourceOutputEntry {
                        source: so.source,
                        owner_module: so.owner_module,
                    },
                );
            }
            ListResult::End | ListResult::Error => {
                let Some(rc) = weak.upgrade() else {
                    return;
                };
                rc.borrow_mut().source_outputs = acc.take();
                Self::finish_refresh(&rc);
            }
        });
    }

    /// Reconciliation pass run once a refresh cycle has landed all lists.
    fn finish_refresh(rc: &Rc<RefCell<Inner>>) {
        Self::check_pending_sources(rc);
        Self::match_pending_loopbacks(rc);
        Self::reconcile_apps(rc);
        Self::check_pending_wires(rc);
        let action = {
            let mut inner = rc.borrow_mut();
            if inner.monitor_out_deferred && Self::source_channels_settled(&inner) {
                inner.monitor_out_deferred = false;
            }
            let usable =
                inner.ready && !inner.monitor_out_pending && !inner.monitor_out_deferred;
            // Also re-run setup when the configured monitor device appeared
            // after monitor-out fell back to another sink.
            let retarget = || {
                let cfg = inner.config.borrow();
                cfg.master.monitor_device.as_ref().is_some_and(|d| {
                    *d != inner.monitor_out.target
                        && inner.sinks.iter().any(|(_, e)| &e.name == d)
                })
            };
            usable && (inner.monitor_out.module.is_none() || retarget())
        };
        if action {
            Self::setup_monitor_output_inner(rc);
        }
        Self::emit(rc, AudioEvent::DevicesChanged);
    }

    /// Every `Source` channel either has its capture loopback running or is
    /// parked waiting for a device that is not on the server.
    fn source_channels_settled(inner: &Inner) -> bool {
        let cfg = inner.config.borrow();
        cfg.channels.iter().all(|c| {
            if !matches!(c.assignment, Some(Assignment::Source { .. })) {
                return true;
            }
            inner.channels.get(&c.id).is_some_and(|rt| {
                rt.pending_source.is_some()
                    || rt.monitor_loop.sink_input.is_some()
                    || rt.input_loop.sink_input.is_some()
            })
        })
    }

    /// Wire channels parked on a capture device that has now appeared. If
    /// the monitor output plays (or is configured to play) on the same card,
    /// tear it down first and let the deferral logic recreate it once the
    /// capture loopbacks run: the Wave XLR's firmware delivers a silent mic
    /// when its playback stream is opened before its capture stream.
    fn check_pending_sources(rc: &Rc<RefCell<Inner>>) {
        let (ready_ids, bounce) = {
            let inner = rc.borrow();
            if !inner.ready || inner.shutting_down {
                return;
            }
            let mut ids = Vec::new();
            let mut cards = Vec::new();
            for (&id, rt) in &inner.channels {
                let Some(p) = &rt.pending_source else {
                    continue;
                };
                if let Some(s) = inner.sources.iter().find(|s| &s.name == p) {
                    ids.push(id);
                    if s.card.is_some() {
                        cards.push(s.card);
                    }
                }
            }
            let bounce = !ids.is_empty() && inner.monitor_out.module.is_some() && {
                let cfg = inner.config.borrow();
                let configured = cfg.master.monitor_device.clone();
                [Some(inner.monitor_out.target.clone()), configured]
                    .iter()
                    .flatten()
                    .any(|t| {
                        let tc = inner
                            .sinks
                            .iter()
                            .find(|(_, e)| &e.name == t)
                            .and_then(|(_, e)| e.card);
                        tc.is_some() && cards.contains(&tc)
                    })
            };
            (ids, bounce)
        };
        if ready_ids.is_empty() {
            return;
        }
        if bounce {
            let old = {
                let mut inner = rc.borrow_mut();
                inner.monitor_out_generation += 1;
                inner.monitor_out_deferred = true;
                let m = inner.monitor_out.module.take();
                if let Some(m) = m {
                    inner.owned_modules.remove(&m);
                }
                inner.monitor_out.sink_input = None;
                inner.monitor_out.source_output = None;
                m
            };
            Self::monitor_out_defer_timeout(rc);
            if let Some(m) = old
                && let Some(mut intro) = Self::introspect(rc) {
                    let _ = intro.unload_module(m, |_| {});
                }
        }
        for id in ready_ids {
            Self::rebuild_channel_inner(rc, id);
        }
    }

    /// Associate freshly loaded loopback modules with the sink-input and
    /// source-output they created, then enforce the configured volume/mute
    /// on every managed loopback that drifted. Session managers (e.g.
    /// WirePlumber's stream-restore) may asynchronously overwrite stream
    /// volumes right after creation; re-checking on every refresh converges
    /// back to our state because we only write on mismatch.
    fn match_pending_loopbacks(rc: &Rc<RefCell<Inner>>) {
        let mut applies: Vec<(Option<u64>, Mix)> = Vec::new();
        let mut input_applies: Vec<u64> = Vec::new();
        let mut moves = DriftMoves::default();
        {
            let mut inner = rc.borrow_mut();
            let by_module: HashMap<u32, (u32, u8)> = inner
                .sink_inputs
                .values()
                .filter_map(|e| e.owner_module.map(|m| (m, (e.index, e.channels))))
                .collect();
            let so_by_module: HashMap<u32, u32> = inner
                .source_outputs
                .iter()
                .filter_map(|(&idx, e)| e.owner_module.map(|m| (m, idx)))
                .collect();
            let ids: Vec<u64> = inner.channels.keys().copied().collect();
            for &id in &ids {
                let Some(rt) = inner.channels.get_mut(&id) else {
                    continue;
                };
                for l in [
                    &mut rt.monitor_loop,
                    &mut rt.stream_loop,
                    &mut rt.vod_loop,
                    &mut rt.input_loop,
                ] {
                    Self::match_loopback(l, &by_module, &so_by_module);
                }
            }
            let mo = &mut inner.monitor_out;
            Self::match_loopback(mo, &by_module, &so_by_module);

            // Detect drift between the server state and our desired state:
            // wrong volume/mute is re-applied, a loopback attached to the
            // wrong sink or capture source is moved back to its target.
            let cfg = inner.config.borrow();
            let sink_names: HashMap<u32, &str> = inner
                .sinks
                .iter()
                .map(|(i, e)| (*i, e.name.as_str()))
                .collect();
            let source_names: HashMap<u32, &str> = inner
                .sources
                .iter()
                .map(|s| (s.index, s.name.as_str()))
                .collect();
            for c in &cfg.channels {
                let Some(rt) = inner.channels.get(&c.id) else {
                    continue;
                };
                // The VOD loopback is a no-op default while the mix is
                // disabled, so it can be checked unconditionally.
                for mix in [Mix::Monitor, Mix::Stream, Mix::Vod] {
                    let l = rt.mix_loop(mix);
                    let (vol, mute) = mix_levels(c, &cfg.master, mix);
                    Self::check_drift(&inner, l, vol, mute, &sink_names, &source_names, &mut moves)
                        .then(|| applies.push((Some(c.id), mix)));
                }
                // The FX input loopback always runs at unity gain.
                if Self::check_drift(
                    &inner,
                    &rt.input_loop,
                    1.0,
                    false,
                    &sink_names,
                    &source_names,
                    &mut moves,
                ) {
                    input_applies.push(c.id);
                }
            }
            if Self::check_drift(
                &inner,
                &inner.monitor_out,
                cfg.master.monitor_volume,
                cfg.master.monitor_muted,
                &sink_names,
                &source_names,
                &mut moves,
            ) {
                applies.push((None, Mix::Monitor));
            }
        }
        if (!moves.sinks.is_empty() || !moves.sources.is_empty())
            && let Some(mut intro) = Self::introspect(rc) {
                for (si, sink) in moves.sinks {
                    let _ = intro.move_sink_input_by_name(si, &sink, None);
                }
                for (so, source) in moves.sources {
                    let _ = intro.move_source_output_by_name(so, &source, None);
                }
            }
        for (id, mix) in applies {
            match id {
                Some(id) => Self::apply_channel_mix_inner(rc, id, mix),
                None => Self::apply_master_monitor_inner(rc),
            }
        }
        for id in input_applies {
            Self::apply_input_loop_inner(rc, id);
        }
    }

    /// Fast path after a loopback module loads: look up only the sink-input
    /// it created and apply the configured volume/mute right away instead of
    /// waiting for the debounced full refresh — the shorter the window where
    /// the stream plays at its default level, the smaller the audible pop
    /// when a channel is (re)wired. No reconciliation happens here:
    /// list-wide healing (drift moves, app stream moves) must only ever run
    /// on a consistent snapshot of all lists, i.e. in finish_refresh.
    fn apply_new_loopback(rc: &Rc<RefCell<Inner>>, which: LoopbackRef, module: u32) {
        let Some(intro) = Self::introspect(rc) else {
            return;
        };
        let weak = Rc::downgrade(rc);
        let found: Rc<Cell<Option<(u32, u8)>>> = Rc::default();
        let _ = intro.get_sink_input_info_list(move |res| match res {
            ListResult::Item(si) => {
                if si.owner_module == Some(module) {
                    found.set(Some((si.index, si.volume.len())));
                }
            }
            ListResult::End | ListResult::Error => {
                let Some(rc) = weak.upgrade() else {
                    return;
                };
                let Some((si, chans)) = found.get() else {
                    return;
                };
                {
                    let mut inner = rc.borrow_mut();
                    let l = match which {
                        LoopbackRef::Mix(id, mix) => {
                            inner.channels.get_mut(&id).map(|rt| rt.mix_loop_mut(mix))
                        }
                        LoopbackRef::Input(id) => {
                            inner.channels.get_mut(&id).map(|rt| &mut rt.input_loop)
                        }
                        LoopbackRef::MonitorOut => Some(&mut inner.monitor_out),
                    };
                    // The loopback may have been rebuilt meanwhile.
                    let Some(l) = l.filter(|l| l.module == Some(module)) else {
                        return;
                    };
                    l.sink_input = Some(si);
                    l.channels = chans;
                }
                match which {
                    LoopbackRef::Mix(id, mix) => Self::apply_channel_mix_inner(&rc, id, mix),
                    LoopbackRef::Input(id) => Self::apply_input_loop_inner(&rc, id),
                    LoopbackRef::MonitorOut => Self::apply_master_monitor_inner(&rc),
                }
            }
        });
    }

    fn match_loopback(
        l: &mut Loopback,
        by_module: &HashMap<u32, (u32, u8)>,
        so_by_module: &HashMap<u32, u32>,
    ) {
        if let (Some(m), None) = (l.module, l.sink_input)
            && let Some(&(si, chans)) = by_module.get(&m) {
                l.sink_input = Some(si);
                l.channels = chans;
            }
        if let (Some(m), None) = (l.module, l.source_output)
            && let Some(&so) = so_by_module.get(&m) {
                l.source_output = Some(so);
            }
    }

    /// Re-assert unity gain on a channel's FX input loopback.
    fn apply_input_loop_inner(rc: &Rc<RefCell<Inner>>, id: u64) {
        let params = {
            let inner = rc.borrow();
            inner
                .channels
                .get(&id)
                .and_then(|rt| rt.input_loop.sink_input.map(|si| (si, rt.input_loop.channels)))
        };
        let Some((si, chans)) = params else {
            return;
        };
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let cv = volume_cv(chans, 1.0);
        let _ = intro.set_sink_input_volume(si, &cv, None);
        let _ = intro.set_sink_input_mute(si, false, None);
    }

    /// Returns true when volume/mute must be re-applied; queues a move when
    /// the loopback sits on the wrong sink, or its capture stream on the
    /// wrong source (only when the intended source actually exists — after
    /// a device replug the stream may briefly point nowhere).
    fn check_drift(
        inner: &Inner,
        l: &Loopback,
        vol: f64,
        mute: bool,
        sink_names: &HashMap<u32, &str>,
        source_names: &HashMap<u32, &str>,
        moves: &mut DriftMoves,
    ) -> bool {
        if !l.source_target.is_empty()
            && let Some(so) = l.source_output
            && let Some(entry) = inner.source_outputs.get(&so)
            && source_names.get(&entry.source).copied() != Some(l.source_target.as_str())
            && inner.sources.iter().any(|s| s.name == l.source_target)
        {
            moves.sources.push((so, l.source_target.clone()));
        }
        let Some(si) = l.sink_input else {
            return false;
        };
        let Some(entry) = inner.sink_inputs.get(&si) else {
            return false;
        };
        if !l.target.is_empty()
            && sink_names.get(&entry.sink).copied() != Some(l.target.as_str())
        {
            moves.sinks.push((si, l.target.clone()));
        }
        let desired = volume_cv(l.channels, vol).avg().0;
        entry.volume_raw.abs_diff(desired) > 1 || entry.mute != mute
    }

    /// Move application streams matching an `App` assignment into that
    /// channel's private sink.
    fn reconcile_apps(rc: &Rc<RefCell<Inner>>) {
        let moves: Vec<(u32, String)> = {
            let inner = rc.borrow();
            let cfg = inner.config.borrow();
            let sink_name_by_index: HashMap<u32, &str> = inner
                .sinks
                .iter()
                .map(|(i, e)| (*i, e.name.as_str()))
                .collect();
            let mut moves = Vec::new();
            for c in &cfg.channels {
                let Some(Assignment::App { name }) = &c.assignment else {
                    continue;
                };
                if inner
                    .channels
                    .get(&c.id)
                    .and_then(|rt| rt.sink_module)
                    .is_none()
                {
                    continue;
                }
                let target = channel_sink_name(c.id);
                if !inner.sinks.iter().any(|(_, e)| e.name == target) {
                    continue;
                }
                for si in inner.sink_inputs.values() {
                    if si
                        .owner_module
                        .is_some_and(|m| inner.owned_modules.contains(&m))
                    {
                        continue;
                    }
                    if si.app_name.as_deref() == Some(name.as_str())
                        && sink_name_by_index.get(&si.sink).copied() != Some(target.as_str())
                    {
                        moves.push((si.index, target.clone()));
                    }
                }
            }
            moves
        };
        if moves.is_empty() {
            return;
        }
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        for (idx, sink) in moves {
            let _ = intro.move_sink_input_by_name(idx, &sink, None);
        }
    }

    fn rebuild_channel_inner(rc: &Rc<RefCell<Inner>>, id: u64) {
        let (channel_cfg, generation_id, to_unload, active) = {
            let mut inner = rc.borrow_mut();
            let rt = inner.channels.entry(id).or_default();
            rt.generation += 1;
            let generation_id = rt.generation;
            rt.pending_wire = None;
            rt.pending_source = None;
            let mut to_unload = Vec::new();
            for l in [
                &mut rt.monitor_loop,
                &mut rt.stream_loop,
                &mut rt.vod_loop,
                &mut rt.input_loop,
            ] {
                if let Some(m) = l.module.take() {
                    to_unload.push(m);
                }
                *l = Loopback::default();
            }
            if let Some(m) = rt.vstin_module.take() {
                to_unload.push(m);
            }
            if let Some(m) = rt.sink_module.take() {
                to_unload.push(m);
            }
            for m in &to_unload {
                inner.owned_modules.remove(m);
            }
            if let Some(s) = inner.peaks.remove(&LevelTarget::Channel(id)) {
                Self::drop_peak(&s);
            }
            let channel_cfg = inner
                .config
                .borrow()
                .channel(id)
                .map(|c| (c.assignment.clone(), c.name.clone()));
            if channel_cfg.is_none() {
                // Channel was removed; stale module callbacks detect the
                // missing runtime entry and clean up after themselves.
                inner.channels.remove(&id);
                inner.fx.remove_channel(id);
            }
            let active = inner.ready && !inner.shutting_down;
            (channel_cfg, generation_id, to_unload, active)
        };
        Self::emit(rc, AudioEvent::Level(LevelTarget::Channel(id), vec![0.0]));
        if let Some(mut intro) = Self::introspect(rc) {
            for m in to_unload {
                let _ = intro.unload_module(m, |_| {});
            }
        }
        if !active {
            return;
        }
        let Some((assignment, channel_name)) = channel_cfg else {
            return;
        };
        match assignment {
            None => {
                let mut inner = rc.borrow_mut();
                inner.fx.ensure_chain(id, None);
                inner.fx.kill_vst(id);
            }
            Some(Assignment::Source { name }) => {
                let present = rc.borrow().sources.iter().any(|s| s.name == name);
                if present {
                    Self::wire_channel_input(rc, id, generation_id, &name);
                } else {
                    // Not on the server (yet): USB mics can enumerate
                    // seconds after login. Park the channel;
                    // check_pending_sources wires it when the device
                    // shows up.
                    if let Some(rt) = rc.borrow_mut().channels.get_mut(&id) {
                        rt.pending_source = Some(name);
                    }
                }
            }
            // App channels and standalone virtual channels both expose a
            // selectable device named after the channel; apps can be routed
            // into it from Crossfade (App) or from the app's own output
            // device picker / OBS audio capture (both).
            Some(Assignment::App { .. }) | Some(Assignment::Virtual) => {
                let sink_name = channel_sink_name(id);
                let clean = sanitize_desc(channel_name.trim());
                let desc = if clean.is_empty() {
                    format!("Crossfade Channel {id}")
                } else {
                    format!("Crossfade: {clean}")
                };
                let args = format!(
                    "sink_name={sink_name} sink_properties='device.description=\"{desc}\"'"
                );
                let Some(mut intro) = Self::introspect(rc) else {
                    return;
                };
                let weak = Rc::downgrade(rc);
                let _ = intro.load_module("module-null-sink", &args, move |idx| {
                    let Some(rc) = weak.upgrade() else {
                        return;
                    };
                    if idx == INVALID_INDEX {
                        return;
                    }
                    let stale = {
                        let inner = rc.borrow();
                        inner
                            .channels
                            .get(&id)
                            .is_none_or(|rt| rt.generation != generation_id)
                            || inner.shutting_down
                    };
                    if stale {
                        if let Some(mut intro) = Self::introspect(&rc) {
                            let _ = intro.unload_module(idx, |_| {});
                        }
                        return;
                    }
                    {
                        let mut inner = rc.borrow_mut();
                        inner.owned_modules.insert(idx);
                        if let Some(rt) = inner.channels.get_mut(&id) {
                            rt.sink_module = Some(idx);
                        }
                    }
                    let monitor = format!("{sink_name}.monitor");
                    Self::wire_channel_input(&rc, id, generation_id, &monitor);
                    Self::schedule_refresh(&rc);
                });
            }
        }
    }

    /// Route a channel's input either straight into the mix loopbacks or
    /// through its FX bridge (Carla rack and/or LV2 filter chain). `source`
    /// is the channel's real input: a capture source or `<sink>.monitor`.
    fn wire_channel_input(rc: &Rc<RefCell<Inner>>, id: u64, generation_id: u64, source: &str) {
        // The first lv2::catalog() call scans every installed bundle, far too
        // slow for the main loop. While the startup warm-up scan is still
        // running, park FX channels until it lands instead of blocking.
        let needs_catalog = {
            let inner = rc.borrow();
            let cfg = inner.config.borrow();
            cfg.channel(id).is_some_and(|c| !c.effects.is_empty())
        };
        if needs_catalog && lv2::scan_pending() {
            let weak = Rc::downgrade(rc);
            let source = source.to_owned();
            lv2::when_ready(move || {
                let Some(rc) = weak.upgrade() else {
                    return;
                };
                let stale = {
                    let inner = rc.borrow();
                    inner.shutting_down
                        || inner
                            .channels
                            .get(&id)
                            .is_none_or(|rt| rt.generation != generation_id)
                };
                if !stale {
                    Self::wire_channel_input(&rc, id, generation_id, &source);
                }
            });
            return;
        }
        let (conf, channel_cfg) = {
            let inner = rc.borrow();
            let cfg = inner.config.borrow();
            let Some(c) = cfg.channel(id) else {
                return;
            };
            // Only touch the catalog when there are effects: for everyone
            // else this must never wait on the warm-up scan.
            let catalog = if c.effects.is_empty() {
                None
            } else {
                lv2::catalog()
            };
            (fx::chain_conf(id, c, catalog.as_deref()), c.clone())
        };
        let Some(conf) = conf else {
            // No active effects: the classic direct wiring.
            {
                let mut inner = rc.borrow_mut();
                inner.fx.ensure_chain(id, None);
                inner.fx.kill_vst(id);
            }
            Self::create_channel_loopbacks(rc, id, generation_id, source);
            Self::create_peak(rc, LevelTarget::Channel(id), source);
            return;
        };
        let use_vst = {
            let mut inner = rc.borrow_mut();
            let use_vst = inner.fx.ensure_vst_host(id, &channel_cfg);
            inner.fx.ensure_chain(id, Some(conf));
            if let Some(rt) = inner.channels.get_mut(&id) {
                if rt.generation != generation_id {
                    return;
                }
                rt.pending_wire = Some(PendingWire {
                    source: source.to_string(),
                    use_vst,
                });
            }
            use_vst
        };
        if use_vst {
            Self::create_vstin_sink(rc, id, generation_id);
        }
        // The bridge nodes appear asynchronously; check_pending_wires picks
        // the channel up on the next device refresh.
        Self::schedule_refresh(rc);
    }

    /// Null sink whose monitor feeds the Carla rack (via pw-link).
    fn create_vstin_sink(rc: &Rc<RefCell<Inner>>, id: u64, generation_id: u64) {
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let sink_name = fx::vstin_sink_name(id);
        let args = format!(
            "sink_name={sink_name} sink_properties='device.description=\"Crossfade Ch {id} VST In (internal)\"'"
        );
        let weak = Rc::downgrade(rc);
        let _ = intro.load_module("module-null-sink", &args, move |idx| {
            let Some(rc) = weak.upgrade() else {
                return;
            };
            if idx == INVALID_INDEX {
                return;
            }
            let stale = {
                let inner = rc.borrow();
                inner
                    .channels
                    .get(&id)
                    .is_none_or(|rt| rt.generation != generation_id)
                    || inner.shutting_down
            };
            if stale {
                if let Some(mut intro) = Self::introspect(&rc) {
                    let _ = intro.unload_module(idx, |_| {});
                }
                return;
            }
            let mut inner = rc.borrow_mut();
            inner.owned_modules.insert(idx);
            if let Some(rt) = inner.channels.get_mut(&id) {
                rt.vstin_module = Some(idx);
            }
            drop(inner);
            Self::schedule_refresh(&rc);
        });
    }

    /// Wire every channel whose FX bridge nodes have appeared on the server
    /// (or whose chain has died, in which case it falls back to the direct
    /// path so the channel is never left silent).
    fn check_pending_wires(rc: &Rc<RefCell<Inner>>) {
        struct Ready {
            id: u64,
            generation: u64,
            source: String,
            use_vst: bool,
            direct: bool,
        }
        let ready: Vec<Ready> = {
            let inner = rc.borrow();
            if !inner.ready || inner.shutting_down {
                return;
            }
            let sink_set: HashSet<&str> =
                inner.sinks.iter().map(|(_, e)| e.name.as_str()).collect();
            let source_set: HashSet<&str> =
                inner.sources.iter().map(|s| s.name.as_str()).collect();
            let mut v = Vec::new();
            for (&id, rt) in &inner.channels {
                let Some(p) = &rt.pending_wire else {
                    continue;
                };
                if inner.fx.chain_failed(id) {
                    v.push(Ready {
                        id,
                        generation: rt.generation,
                        source: p.source.clone(),
                        use_vst: false,
                        direct: true,
                    });
                    continue;
                }
                if !sink_set.contains(fx::chain_sink_name(id).as_str())
                    || !source_set.contains(fx::chain_source_name(id).as_str())
                {
                    continue;
                }
                let use_vst = p.use_vst && inner.fx.vst_running(id);
                if use_vst && !sink_set.contains(fx::vstin_sink_name(id).as_str()) {
                    continue;
                }
                v.push(Ready {
                    id,
                    generation: rt.generation,
                    source: p.source.clone(),
                    use_vst,
                    direct: false,
                });
            }
            v
        };
        for r in ready {
            {
                let mut inner = rc.borrow_mut();
                let Some(rt) = inner.channels.get_mut(&r.id) else {
                    continue;
                };
                if rt.generation != r.generation {
                    continue;
                }
                rt.pending_wire = None;
            }
            if r.direct {
                Self::create_channel_loopbacks(rc, r.id, r.generation, &r.source);
                Self::create_peak(rc, LevelTarget::Channel(r.id), &r.source);
                continue;
            }
            let bridge_in = if r.use_vst {
                fx::vstin_sink_name(r.id)
            } else {
                fx::chain_sink_name(r.id)
            };
            let post = fx::chain_source_name(r.id);
            Self::create_input_loopback(rc, r.id, r.generation, &r.source, &bridge_in);
            Self::create_channel_loopbacks(rc, r.id, r.generation, &post);
            Self::create_peak(rc, LevelTarget::Channel(r.id), &post);
            if r.use_vst {
                rc.borrow_mut().fx.start_vst_links(r.id);
            }
        }
    }

    /// Unity-gain loopback feeding the channel input into the FX bridge.
    fn create_input_loopback(
        rc: &Rc<RefCell<Inner>>,
        id: u64,
        generation_id: u64,
        source: &str,
        sink: &str,
    ) {
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let tag = format!("Crossfade ch{id} fx-in");
        let args = loopback_args(source, sink, &tag);
        let sink = sink.to_string();
        let source = source.to_string();
        let weak = Rc::downgrade(rc);
        let _ = intro.load_module("module-loopback", &args, move |idx| {
            let Some(rc) = weak.upgrade() else {
                return;
            };
            if idx == INVALID_INDEX {
                return;
            }
            let stale = {
                let inner = rc.borrow();
                inner
                    .channels
                    .get(&id)
                    .is_none_or(|rt| rt.generation != generation_id)
                    || inner.shutting_down
            };
            if stale {
                if let Some(mut intro) = Self::introspect(&rc) {
                    let _ = intro.unload_module(idx, |_| {});
                }
                return;
            }
            {
                let mut inner = rc.borrow_mut();
                inner.owned_modules.insert(idx);
                if let Some(rt) = inner.channels.get_mut(&id) {
                    rt.input_loop = Loopback {
                        module: Some(idx),
                        target: sink.clone(),
                        source_target: source.clone(),
                        ..Loopback::default()
                    };
                }
            }
            Self::apply_new_loopback(&rc, LoopbackRef::Input(id), idx);
            Self::schedule_refresh(&rc);
        });
    }

    fn create_channel_loopbacks(rc: &Rc<RefCell<Inner>>, id: u64, generation_id: u64, source: &str) {
        let vod = rc.borrow().config.borrow().vod_mix_enabled;
        for mix in [Mix::Monitor, Mix::Stream] {
            Self::create_mix_loopback(rc, id, generation_id, source, mix);
        }
        if vod {
            Self::create_mix_loopback(rc, id, generation_id, source, Mix::Vod);
        }
    }

    /// One channel's send into one mix bus.
    fn create_mix_loopback(
        rc: &Rc<RefCell<Inner>>,
        id: u64,
        generation_id: u64,
        source: &str,
        mix: Mix,
    ) {
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let sink = match mix {
            Mix::Monitor => MONITOR_SINK,
            Mix::Stream => STREAM_SINK,
            Mix::Vod => VOD_SINK,
        };
        let tag = format!(
            "Crossfade ch{id} {}",
            match mix {
                Mix::Monitor => "monitor",
                Mix::Stream => "stream",
                Mix::Vod => "vod",
            }
        );
        let args = loopback_args(source, sink, &tag);
        // Mark the load as in flight (a non-empty target with no module):
        // apply_vod_mix_inner uses this to avoid double-creating a send
        // whose module callback has not landed yet.
        if let Some(rt) = rc.borrow_mut().channels.get_mut(&id) {
            rt.mix_loop_mut(mix).target = sink.to_string();
        }
        let source = source.to_string();
        let weak = Rc::downgrade(rc);
        let _ = intro.load_module("module-loopback", &args, move |idx| {
            let Some(rc) = weak.upgrade() else {
                return;
            };
            if idx == INVALID_INDEX {
                return;
            }
            let stale = {
                let inner = rc.borrow();
                inner
                    .channels
                    .get(&id)
                    .is_none_or(|rt| rt.generation != generation_id)
                    || inner.shutting_down
                    // The VOD mix may have been disabled (and its bus
                    // unloaded) while this load was in flight.
                    || (mix == Mix::Vod && !inner.config.borrow().vod_mix_enabled)
            };
            if stale {
                if let Some(mut intro) = Self::introspect(&rc) {
                    let _ = intro.unload_module(idx, |_| {});
                }
                return;
            }
            {
                let mut inner = rc.borrow_mut();
                inner.owned_modules.insert(idx);
                if let Some(rt) = inner.channels.get_mut(&id) {
                    *rt.mix_loop_mut(mix) = Loopback {
                        module: Some(idx),
                        target: sink.to_string(),
                        source_target: source.clone(),
                        ..Loopback::default()
                    };
                }
            }
            Self::apply_new_loopback(&rc, LoopbackRef::Mix(id, mix), idx);
            Self::schedule_refresh(&rc);
        });
    }

    fn setup_monitor_output_inner(rc: &Rc<RefCell<Inner>>) {
        let (generation_id, old, target) = {
            let mut inner = rc.borrow_mut();
            if !inner.ready || inner.shutting_down {
                return;
            }
            inner.monitor_out_generation += 1;
            inner.monitor_out_pending = false;
            inner.monitor_out_deferred = false;
            let old = inner.monitor_out.module.take();
            if let Some(m) = old {
                inner.owned_modules.remove(&m);
            }
            inner.monitor_out.sink_input = None;
            inner.monitor_out.source_output = None;
            let configured = inner.config.borrow().master.monitor_device.clone();
            let target = configured
                .filter(|d| inner.sinks.iter().any(|(_, e)| &e.name == d))
                .or_else(|| {
                    inner
                        .default_sink
                        .clone()
                        .filter(|d| !d.starts_with(OWN_PREFIX))
                });
            (inner.monitor_out_generation, old, target)
        };
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        if let Some(m) = old {
            let _ = intro.unload_module(m, |_| {});
        }
        let Some(target) = target else {
            return;
        };
        rc.borrow_mut().monitor_out_pending = true;
        let args = loopback_args(
            &format!("{MONITOR_SINK}.monitor"),
            &target,
            "Crossfade monitor-out",
        );
        let weak = Rc::downgrade(rc);
        let _ = intro.load_module("module-loopback", &args, move |idx| {
            let Some(rc) = weak.upgrade() else {
                return;
            };
            let stale = {
                let mut inner = rc.borrow_mut();
                inner.monitor_out_pending = false;
                inner.monitor_out_generation != generation_id || inner.shutting_down
            };
            if idx == INVALID_INDEX {
                return;
            }
            if stale {
                if let Some(mut intro) = Self::introspect(&rc) {
                    let _ = intro.unload_module(idx, |_| {});
                }
                return;
            }
            {
                let mut inner = rc.borrow_mut();
                inner.owned_modules.insert(idx);
                inner.monitor_out = Loopback {
                    module: Some(idx),
                    target: target.clone(),
                    source_target: format!("{MONITOR_SINK}.monitor"),
                    ..Loopback::default()
                };
            }
            Self::apply_new_loopback(&rc, LoopbackRef::MonitorOut, idx);
            Self::schedule_refresh(&rc);
        });
    }

    fn apply_vod_mix_inner(rc: &Rc<RefCell<Inner>>) {
        let (usable, enabled, active) = {
            let inner = rc.borrow();
            (
                inner.ready && !inner.shutting_down,
                inner.config.borrow().vod_mix_enabled,
                !inner.vod_modules.is_empty(),
            )
        };
        // Before the bootstrap the config flag alone decides what gets
        // created, so there is nothing to reconcile yet.
        if !usable || enabled == active {
            return;
        }
        if !enabled {
            let to_unload = {
                let mut inner = rc.borrow_mut();
                let mut mods: Vec<u32> = Vec::new();
                for rt in inner.channels.values_mut() {
                    if let Some(m) = rt.vod_loop.module.take() {
                        mods.push(m);
                    }
                    rt.vod_loop = Loopback::default();
                }
                mods.append(&mut inner.vod_modules);
                for m in &mods {
                    inner.owned_modules.remove(m);
                }
                if let Some(s) = inner.peaks.remove(&LevelTarget::VodMix) {
                    Self::drop_peak(&s);
                }
                mods
            };
            Self::emit(rc, AudioEvent::Level(LevelTarget::VodMix, vec![0.0]));
            if let Some(mut intro) = Self::introspect(rc) {
                for m in to_unload {
                    let _ = intro.unload_module(m, |_| {});
                }
            }
            return;
        }
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let args = format!(
            "sink_name={VOD_SINK} sink_properties='device.description=\"Crossfade VOD Bus\" device.icon_name=audio-input-microphone'"
        );
        let weak = Rc::downgrade(rc);
        let _ = intro.load_module("module-null-sink", &args, move |idx| {
            let Some(rc) = weak.upgrade() else {
                return;
            };
            if idx == INVALID_INDEX {
                return;
            }
            let stale = {
                let mut inner = rc.borrow_mut();
                let stale =
                    inner.shutting_down || !inner.config.borrow().vod_mix_enabled;
                if !stale {
                    inner.owned_modules.insert(idx);
                    inner.vod_modules.push(idx);
                }
                stale
            };
            if stale {
                if let Some(mut intro) = Self::introspect(&rc) {
                    let _ = intro.unload_module(idx, |_| {});
                }
                return;
            }
            Self::create_mix_mic(&rc, VOD_SINK, VOD_MIC, "Crossfade VOD Mix");
            Self::create_peak(&rc, LevelTarget::VodMix, &format!("{VOD_SINK}.monitor"));
            // Add the VOD send to every channel that is already wired;
            // parked/pending channels pick it up when they wire. `target` is
            // set eagerly by create_mix_loopback, so a send whose module
            // load is still in flight is not created twice.
            let wired: Vec<(u64, u64, String)> = {
                let inner = rc.borrow();
                inner
                    .channels
                    .iter()
                    .filter_map(|(&id, rt)| {
                        let source = &rt.monitor_loop.source_target;
                        (!source.is_empty()
                            && rt.vod_loop.module.is_none()
                            && rt.vod_loop.target.is_empty())
                        .then(|| (id, rt.generation, source.clone()))
                    })
                    .collect()
            };
            for (id, generation, source) in wired {
                Self::create_mix_loopback(&rc, id, generation, &source, Mix::Vod);
            }
            Self::schedule_refresh(&rc);
        });
    }

    fn apply_channel_mix_inner(rc: &Rc<RefCell<Inner>>, id: u64, mix: Mix) {
        let params = {
            let inner = rc.borrow();
            let cfg = inner.config.borrow();
            let Some(c) = cfg.channel(id) else {
                return;
            };
            let Some(rt) = inner.channels.get(&id) else {
                return;
            };
            let l = rt.mix_loop(mix);
            l.sink_input.map(|si| {
                let (vol, mute) = mix_levels(c, &cfg.master, mix);
                (si, l.channels, vol, mute)
            })
        };
        let Some((si, chans, vol, mute)) = params else {
            return;
        };
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let cv = volume_cv(chans, vol);
        let _ = intro.set_sink_input_volume(si, &cv, None);
        let _ = intro.set_sink_input_mute(si, mute, None);
    }

    fn apply_master_monitor_inner(rc: &Rc<RefCell<Inner>>) {
        let params = {
            let inner = rc.borrow();
            let cfg = inner.config.borrow();
            inner
                .monitor_out
                .sink_input
                .map(|si| {
                    (
                        si,
                        inner.monitor_out.channels,
                        cfg.master.monitor_volume,
                        cfg.master.monitor_muted,
                    )
                })
        };
        let Some((si, chans, vol, mute)) = params else {
            return;
        };
        let Some(mut intro) = Self::introspect(rc) else {
            return;
        };
        let cv = volume_cv(chans, vol);
        let _ = intro.set_sink_input_volume(si, &cv, None);
        let _ = intro.set_sink_input_mute(si, mute, None);
    }

    /// Attach a low-rate peak-detect record stream (the pavucontrol trick) to
    /// `source` and forward its levels as events. The source's channel count
    /// is looked up first so stereo inputs are metered per channel — a mono
    /// peek stream would average the channels and under-read panned signals.
    fn create_peak(rc: &Rc<RefCell<Inner>>, target: LevelTarget, source: &str) {
        let Some(intro) = Self::introspect(rc) else {
            return;
        };
        let weak = Rc::downgrade(rc);
        let source_owned = source.to_string();
        intro.get_source_info_by_name(source, move |res| {
            if let ListResult::Item(info) = res {
                let channels = info.sample_spec.channels.clamp(1, 2);
                if let Some(rc) = weak.upgrade() {
                    Self::create_peak_stream(&rc, target, &source_owned, channels);
                }
            }
        });
    }

    fn create_peak_stream(
        rc: &Rc<RefCell<Inner>>,
        target: LevelTarget,
        source: &str,
        channels: u8,
    ) {
        let stream = {
            let mut inner = rc.borrow_mut();
            if !inner.ready || inner.shutting_down {
                return;
            }
            if let Some(old) = inner.peaks.remove(&target) {
                Self::drop_peak(&old);
            }
            let Some(ctx) = inner.context.as_mut() else {
                return;
            };
            let spec = Spec {
                format: Format::FLOAT32NE,
                channels,
                rate: 25,
            };
            // Unique media.name per meter plus restore opt-outs; otherwise
            // WirePlumber remembers one shared "Peak Detect" entry and moves
            // every meter onto the same source.
            let name = match target {
                LevelTarget::Channel(i) => format!("Crossfade meter ch{i}"),
                LevelTarget::MonitorMix => "Crossfade meter monitor".to_string(),
                LevelTarget::StreamMix => "Crossfade meter stream".to_string(),
                LevelTarget::VodMix => "Crossfade meter vod".to_string(),
            };
            let mut props = Proplist::new().expect("proplist");
            let _ = props.set_str("media.name", &name);
            let _ = props.set_str("state.restore-props", "false");
            let _ = props.set_str("state.restore-target", "false");
            match Stream::new_with_proplist(ctx, &name, &spec, None, &mut props) {
                Some(s) => s,
                None => return,
            }
        };
        let stream = Rc::new(RefCell::new(stream));
        let weak_stream = Rc::downgrade(&stream);
        let weak_inner = Rc::downgrade(rc);
        stream
            .borrow_mut()
            .set_read_callback(Some(Box::new(move |_len| {
                let Some(s) = weak_stream.upgrade() else {
                    return;
                };
                let n = usize::from(channels);
                let mut peaks = [0.0f32; 2];
                let mut got_data = false;
                {
                    let mut st = s.borrow_mut();
                    loop {
                        match st.peek() {
                            Ok(PeekResult::Data(data)) => {
                                for frame in data.chunks_exact(4 * n) {
                                    for (i, chunk) in frame.chunks_exact(4).enumerate() {
                                        let v = f32::from_ne_bytes(chunk.try_into().unwrap())
                                            .abs();
                                        if v > peaks[i] {
                                            peaks[i] = v;
                                        }
                                    }
                                }
                                got_data = true;
                                let _ = st.discard();
                            }
                            Ok(PeekResult::Hole(_)) => {
                                got_data = true;
                                let _ = st.discard();
                            }
                            Ok(PeekResult::Empty) | Err(_) => break,
                        }
                    }
                }
                if got_data
                    && let Some(rc) = weak_inner.upgrade() {
                        let levels = peaks[..n]
                            .iter()
                            .map(|p| f64::from(p.clamp(0.0, 1.0)))
                            .collect();
                        Self::emit(&rc, AudioEvent::Level(target, levels));
                    }
            })));
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: 4 * u32::from(channels),
        };
        let ok = stream
            .borrow_mut()
            .connect_record(
                Some(source),
                Some(&attr),
                StreamFlagSet::PEAK_DETECT
                    | StreamFlagSet::ADJUST_LATENCY
                    | StreamFlagSet::DONT_INHIBIT_AUTO_SUSPEND,
            )
            .is_ok();
        if ok {
            rc.borrow_mut().peaks.insert(target, stream);
        }
    }
}
