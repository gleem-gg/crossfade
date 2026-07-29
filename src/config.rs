use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

use gtk::glib;
use serde::{Deserialize, Serialize};

pub const MAX_CHANNELS: usize = 8;

/// Default names offered when adding channels via "+".
pub const TEMPLATE_NAMES: [&str; 6] = ["Game", "Music", "Voice Chat", "Browser", "SFX", "Aux"];

/// What feeds an input channel: a capture source (hardware input or a monitor
/// of another device), an application's playback stream matched by its
/// `application.name`, or a standalone virtual device ("Crossfade: <name>")
/// that other software can select as an output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Assignment {
    Source { name: String },
    App { name: String },
    Virtual,
}

/// One LV2 plugin instance in a channel's effect chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EffectConfig {
    /// Stable identifier, unique within the channel; used as the node label
    /// in the generated filter-chain graph, so it must survive reordering.
    pub id: u64,
    /// LV2 plugin URI.
    pub uri: String,
    /// Display name (cached from the plugin so the UI works even when the
    /// plugin is uninstalled later).
    pub name: String,
    pub enabled: bool,
    /// Control-port values by port symbol; ports not listed here keep the
    /// plugin's default.
    pub controls: BTreeMap<String, f64>,
}

impl Default for EffectConfig {
    fn default() -> Self {
        Self {
            id: 0,
            uri: String::new(),
            name: String::new(),
            enabled: true,
            controls: BTreeMap::new(),
        }
    }
}

/// One VST2/VST3 plugin in a channel's rack, hosted by the helper process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VstPluginConfig {
    /// Stable identifier, unique within the channel.
    pub id: u64,
    /// Path to the .so / .vst3 binary.
    pub path: String,
    pub format: crate::vst::VstFormat,
    /// Sub-plugin label inside a multi-plugin bundle (VST3); empty otherwise.
    pub label: String,
    /// Sub-plugin selector for multi-plugin binaries (0 = whole file).
    pub unique_id: i64,
    /// Display name (cached from discovery).
    pub name: String,
    pub enabled: bool,
    /// Parameter values by parameter index (as decimal string keys, since
    /// JSON object keys are strings).
    pub params: BTreeMap<String, f64>,
}

impl Default for VstPluginConfig {
    fn default() -> Self {
        Self {
            id: 0,
            path: String::new(),
            format: crate::vst::VstFormat::Vst2,
            label: String::new(),
            unique_id: 0,
            name: String::new(),
            enabled: true,
            params: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelConfig {
    /// Stable identifier; never reused, survives reordering and removal.
    pub id: u64,
    pub name: String,
    pub assignment: Option<Assignment>,
    pub monitor_volume: f64,
    pub stream_volume: f64,
    pub vod_volume: f64,
    pub monitor_muted: bool,
    pub stream_muted: bool,
    pub vod_muted: bool,
    /// Links all of the channel's faders (including the VOD fader while the
    /// VOD mix is enabled).
    pub linked: bool,
    /// Accent color ("#rrggbb") marking the channel: shown on the strip's
    /// color button and on any MIDI pad bound as the channel's color pad.
    /// `None` = no color assigned.
    pub color: Option<String>,
    /// Built-in channels (Microphone, System) that cannot be removed.
    pub permanent: bool,
    /// LV2 effect chain applied to this channel's input before it reaches
    /// the monitor/stream mixes.
    pub effects: Vec<EffectConfig>,
    /// Next effect/VST id for this channel; never reused, shared by both
    /// lists.
    pub next_effect_id: u64,
    /// VST rack processed in front of the LV2 chain.
    pub vst_plugins: Vec<VstPluginConfig>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            assignment: None,
            monitor_volume: 0.75,
            stream_volume: 0.75,
            vod_volume: 0.75,
            monitor_muted: false,
            stream_muted: false,
            vod_muted: false,
            linked: false,
            color: None,
            permanent: false,
            effects: Vec::new(),
            next_effect_id: 1,
            vst_plugins: Vec::new(),
        }
    }
}

impl ChannelConfig {
    /// The channel color as 8-bit RGB components.
    pub fn color_rgb(&self) -> Option<(u8, u8, u8)> {
        let hex = self.color.as_deref()?.strip_prefix('#')?;
        if hex.len() != 6 {
            return None;
        }
        let n = u32::from_str_radix(hex, 16).ok()?;
        Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
    }

    /// Effects that should actually be instantiated in the chain.
    pub fn enabled_effects(&self) -> Vec<&EffectConfig> {
        self.effects.iter().filter(|e| e.enabled).collect()
    }

    /// VST plugins that should actually be loaded into the rack.
    pub fn enabled_vsts(&self) -> Vec<&VstPluginConfig> {
        self.vst_plugins.iter().filter(|p| p.enabled).collect()
    }

    /// Whether this channel routes through an FX bridge at all.
    pub fn fx_active(&self) -> bool {
        self.effects.iter().any(|e| e.enabled)
            || self.vst_plugins.iter().any(|p| p.enabled)
    }

    /// Append an effect and return a reference to it.
    pub fn add_effect(&mut self, uri: &str, name: &str) -> &EffectConfig {
        let id = self.next_effect_id;
        self.next_effect_id += 1;
        self.effects.push(EffectConfig {
            id,
            uri: uri.to_string(),
            name: name.to_string(),
            ..EffectConfig::default()
        });
        self.effects.last().unwrap()
    }

    pub fn effect_mut(&mut self, effect_id: u64) -> Option<&mut EffectConfig> {
        self.effects.iter_mut().find(|e| e.id == effect_id)
    }

    /// Append a VST plugin from a discovery entry.
    pub fn add_vst(&mut self, entry: &crate::vst::VstEntry) -> u64 {
        let id = self.next_effect_id;
        self.next_effect_id += 1;
        self.vst_plugins.push(VstPluginConfig {
            id,
            path: entry.path.clone(),
            format: entry.format,
            label: entry.label.clone(),
            unique_id: entry.unique_id,
            name: entry.name.clone(),
            ..VstPluginConfig::default()
        });
        id
    }

    pub fn vst_mut(&mut self, vst_id: u64) -> Option<&mut VstPluginConfig> {
        self.vst_plugins.iter_mut().find(|p| p.id == vst_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MasterConfig {
    /// Hardware sink the monitor mix is routed to; `None` = system default.
    pub monitor_device: Option<String>,
    pub monitor_volume: f64,
    pub stream_volume: f64,
    pub vod_volume: f64,
    pub monitor_muted: bool,
    pub stream_muted: bool,
    pub vod_muted: bool,
}

impl Default for MasterConfig {
    fn default() -> Self {
        Self {
            monitor_device: None,
            monitor_volume: 1.0,
            stream_volume: 1.0,
            vod_volume: 1.0,
            monitor_muted: false,
            stream_muted: false,
            vod_muted: false,
        }
    }
}

/// Volume levels re-applied to an Elgato Wave XLR every time Crossfade starts
/// (the device occasionally forgets its volume settings). Percentages 0–100;
/// `None` = leave that control alone.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WaveXlrConfig {
    pub mic_volume: Option<f64>,
    pub output_volume: Option<f64>,
}

/// What kind of MIDI message a binding listens for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiKind {
    Cc,
    Note,
}

/// One physical control, identified by controller name (the USB product
/// string — stable across replugs, unlike sequencer client ids) plus the
/// message's MIDI channel and CC/note number.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MidiSource {
    pub device: String,
    pub channel: u8,
    pub kind: MidiKind,
    pub number: u8,
}

impl MidiSource {
    /// Short human-readable form, e.g. "CC 48 · APC MINI mk2".
    pub fn label(&self) -> String {
        let what = match self.kind {
            MidiKind::Cc => "CC",
            MidiKind::Note => "Note",
        };
        format!("{what} {} · {}", self.number, self.device)
    }
}

/// What a bound control drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum MidiTarget {
    ChannelVolume { id: u64, mix: crate::audio::Mix },
    ChannelMute { id: u64, mix: crate::audio::Mix },
    /// A pad lit in the channel's color, as a locator on the controller;
    /// pressing it does nothing.
    ChannelColor { id: u64 },
    MasterVolume { mix: crate::audio::Mix },
    MasterMute { mix: crate::audio::Mix },
    /// Switches the active binding profile (by stable profile id). Stored
    /// in `MidiConfig::global_bindings` so profile pads work everywhere.
    SelectProfile { profile: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MidiBinding {
    pub source: MidiSource,
    pub target: MidiTarget,
}

/// A bank of bindings; pads bound to `SelectProfile` switch between them
/// (e.g. one profile per mix layer, or one per channel).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiProfile {
    /// Stable identifier; never reused, survives deletion of other profiles.
    pub id: u64,
    pub name: String,
    pub bindings: Vec<MidiBinding>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiConfig {
    /// Fader pickup: after a profile switch a hardware fader only takes
    /// effect once it crosses the target's current value, instead of
    /// jumping the volume to wherever the fader happens to sit.
    pub pickup: bool,
    pub next_profile_id: u64,
    pub profiles: Vec<MidiProfile>,
    /// Id of the profile whose bindings are live; persisted so a restart
    /// comes back in the same bank.
    pub active_profile: u64,
    /// Profile-select pads, active regardless of the current profile.
    pub global_bindings: Vec<MidiBinding>,
    /// Light up note-bound pads (mute state, active profile) by sending
    /// note-ons back to the controller.
    pub led_feedback: bool,
    /// Velocity sent for a lit pad; selects the color on e.g. an APC mini.
    pub on_velocity: u8,
    /// Velocity sent for a dark pad.
    pub off_velocity: u8,
}

impl Default for MidiConfig {
    fn default() -> Self {
        Self {
            pickup: true,
            next_profile_id: 2,
            profiles: vec![MidiProfile {
                id: 1,
                name: "Default".to_string(),
                ..MidiProfile::default()
            }],
            active_profile: 1,
            global_bindings: Vec::new(),
            led_feedback: true,
            on_velocity: 127,
            off_velocity: 0,
        }
    }
}

impl MidiConfig {
    pub fn active(&self) -> &MidiProfile {
        self.profiles
            .iter()
            .find(|p| p.id == self.active_profile)
            .unwrap_or(&self.profiles[0])
    }

    pub fn profile(&self, id: u64) -> Option<&MidiProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn add_profile(&mut self) -> u64 {
        let id = self.next_profile_id;
        self.next_profile_id += 1;
        self.profiles.push(MidiProfile {
            id,
            name: format!("Profile {id}"),
            ..MidiProfile::default()
        });
        id
    }

    /// Remove a profile plus every pad bound to it; keeps at least one
    /// profile and repairs `active_profile` if it pointed at the removed one.
    pub fn remove_profile(&mut self, id: u64) {
        if self.profiles.len() <= 1 {
            return;
        }
        self.profiles.retain(|p| p.id != id);
        self.global_bindings
            .retain(|b| !matches!(b.target, MidiTarget::SelectProfile { profile } if profile == id));
        if self.profile(self.active_profile).is_none() {
            self.active_profile = self.profiles[0].id;
        }
    }

    /// Store a binding in the given profile (or in the global list for
    /// `SelectProfile` targets), evicting any binding there that already
    /// uses the same physical control or the same target — one control per
    /// target, one target per control.
    pub fn bind(&mut self, profile_id: u64, binding: MidiBinding) {
        let list = if matches!(binding.target, MidiTarget::SelectProfile { .. }) {
            &mut self.global_bindings
        } else {
            let Some(p) = self.profiles.iter_mut().find(|p| p.id == profile_id) else {
                return;
            };
            &mut p.bindings
        };
        list.retain(|b| b.source != binding.source && b.target != binding.target);
        list.push(binding);
    }

    /// Remove the binding for a target from the given profile (globals for
    /// `SelectProfile` targets).
    pub fn unbind(&mut self, profile_id: u64, target: &MidiTarget) {
        if matches!(target, MidiTarget::SelectProfile { .. }) {
            self.global_bindings.retain(|b| b.target != *target);
        } else if let Some(p) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
            p.bindings.retain(|b| b.target != *target);
        }
    }

    /// Drop every binding that references a removed channel.
    pub fn remove_channel_bindings(&mut self, channel_id: u64) {
        let refers = |t: &MidiTarget| {
            matches!(
                t,
                MidiTarget::ChannelVolume { id, .. }
                    | MidiTarget::ChannelMute { id, .. }
                    | MidiTarget::ChannelColor { id }
                    if *id == channel_id
            )
        };
        for p in &mut self.profiles {
            p.bindings.retain(|b| !refers(&b.target));
        }
        self.global_bindings.retain(|b| !refers(&b.target));
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub next_channel_id: u64,
    pub channels: Vec<ChannelConfig>,
    pub master: MasterConfig,
    /// Third mix bus exposed as the "Crossfade VOD Mix" microphone, for
    /// keeping e.g. music out of the VOD/recording track. Off by default so
    /// the two-mix UI stays clean.
    pub vod_mix_enabled: bool,
    /// The setup assistant was shown once; afterwards misconfigurations only
    /// produce a notice instead of the full dialog.
    pub setup_done: bool,
    pub wave_xlr: WaveXlrConfig,
    pub midi: MidiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            next_channel_id: 3,
            channels: vec![
                ChannelConfig {
                    id: 1,
                    name: "Microphone".to_string(),
                    permanent: true,
                    ..ChannelConfig::default()
                },
                ChannelConfig {
                    id: 2,
                    name: "System".to_string(),
                    assignment: Some(Assignment::Virtual),
                    permanent: true,
                    ..ChannelConfig::default()
                },
            ],
            master: MasterConfig::default(),
            vod_mix_enabled: false,
            setup_done: false,
            wave_xlr: WaveXlrConfig::default(),
            midi: MidiConfig::default(),
        }
    }
}

/// One-time migration from the pre-rename OpenWave identity: copies the old
/// config directory (config.json, fx/, vst-state/) if the new one doesn't
/// exist yet. The old directory is left in place as a downgrade path.
/// Returns whether a legacy autostart entry existed (it is removed here; the
/// caller recreates it under the new app id).
pub fn migrate_from_openwave() -> bool {
    let config_root = glib::user_config_dir();
    let new_dir = config_root.join("gleem-crossfade");
    let old_dir = config_root.join("openwave");
    if !new_dir.exists()
        && old_dir.is_dir()
        && let Err(e) = copy_dir(&old_dir, &new_dir)
    {
        eprintln!("gleem-crossfade: could not migrate the OpenWave config: {e}");
    }
    let old_autostart = config_root
        .join("autostart")
        .join("de.ghostzero.OpenWave.desktop");
    if old_autostart.exists() {
        let _ = fs::remove_file(&old_autostart);
        return true;
    }
    false
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

impl Config {
    fn path() -> PathBuf {
        glib::user_config_dir().join("gleem-crossfade").join("config.json")
    }

    pub fn load() -> Self {
        let mut cfg: Config = fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        // Migration from the fixed-8-channel format (no ids): drop the
        // untouched filler channels, keep anything assigned plus the two
        // defaults.
        if cfg.channels.iter().any(|c| c.id == 0) {
            cfg.channels.retain(|c| {
                c.assignment.is_some() || c.name == "Microphone" || c.name == "System"
            });
        }

        let mut seen: HashSet<u64> = HashSet::new();
        let mut max_id = 0;
        for ch in &mut cfg.channels {
            if ch.id == 0 || seen.contains(&ch.id) {
                ch.id = cfg.next_channel_id.max(max_id + 1);
            }
            seen.insert(ch.id);
            max_id = max_id.max(ch.id);
            if ch.name.trim().is_empty() {
                ch.name = format!("Channel {}", ch.id);
            }
            // Idempotent: also upgrades configs saved before the flag existed.
            if ch.name == "Microphone" || ch.name == "System" {
                ch.permanent = true;
            }
            ch.monitor_volume = ch.monitor_volume.clamp(0.0, 1.0);
            ch.stream_volume = ch.stream_volume.clamp(0.0, 1.0);
            ch.vod_volume = ch.vod_volume.clamp(0.0, 1.0);
            // Hand-edited colors that are not "#rrggbb" are dropped.
            if ch.color.is_some() && ch.color_rgb().is_none() {
                ch.color = None;
            }
            // Repair effect/VST ids the same way as channel ids (they share
            // one id space per channel).
            let mut seen_fx: HashSet<u64> = HashSet::new();
            let mut max_fx = 0;
            for fx in &mut ch.effects {
                if fx.id == 0 || seen_fx.contains(&fx.id) {
                    fx.id = ch.next_effect_id.max(max_fx + 1);
                }
                seen_fx.insert(fx.id);
                max_fx = max_fx.max(fx.id);
            }
            for p in &mut ch.vst_plugins {
                if p.id == 0 || seen_fx.contains(&p.id) {
                    p.id = ch.next_effect_id.max(max_fx + 1);
                }
                seen_fx.insert(p.id);
                max_fx = max_fx.max(p.id);
            }
            ch.effects.retain(|fx| !fx.uri.is_empty());
            ch.vst_plugins.retain(|p| !p.path.is_empty());
            ch.next_effect_id = ch.next_effect_id.max(max_fx + 1);
        }
        cfg.channels.truncate(MAX_CHANNELS);
        cfg.next_channel_id = cfg.next_channel_id.max(max_id + 1);
        cfg.master.monitor_volume = cfg.master.monitor_volume.clamp(0.0, 1.0);
        cfg.master.stream_volume = cfg.master.stream_volume.clamp(0.0, 1.0);
        cfg.master.vod_volume = cfg.master.vod_volume.clamp(0.0, 1.0);
        cfg.wave_xlr.mic_volume = cfg.wave_xlr.mic_volume.map(|v| v.clamp(0.0, 100.0));
        cfg.wave_xlr.output_volume = cfg.wave_xlr.output_volume.map(|v| v.clamp(0.0, 100.0));

        // MIDI: repair profile ids the same way as channel ids, then drop
        // bindings that reference channels or profiles that no longer exist
        // (hand-edited configs, channels removed by older builds).
        if cfg.midi.profiles.is_empty() {
            cfg.midi.profiles = MidiConfig::default().profiles;
        }
        let mut seen_p: HashSet<u64> = HashSet::new();
        let mut max_p = 0;
        for p in &mut cfg.midi.profiles {
            if p.id == 0 || seen_p.contains(&p.id) {
                p.id = cfg.midi.next_profile_id.max(max_p + 1);
            }
            seen_p.insert(p.id);
            max_p = max_p.max(p.id);
            if p.name.trim().is_empty() {
                p.name = format!("Profile {}", p.id);
            }
        }
        cfg.midi.next_profile_id = cfg.midi.next_profile_id.max(max_p + 1);
        if cfg.midi.profile(cfg.midi.active_profile).is_none() {
            cfg.midi.active_profile = cfg.midi.profiles[0].id;
        }
        let channel_ids: HashSet<u64> = cfg.channels.iter().map(|c| c.id).collect();
        let valid = |t: &MidiTarget| match t {
            MidiTarget::ChannelVolume { id, .. }
            | MidiTarget::ChannelMute { id, .. }
            | MidiTarget::ChannelColor { id } => channel_ids.contains(id),
            MidiTarget::SelectProfile { profile } => seen_p.contains(profile),
            MidiTarget::MasterVolume { .. } | MidiTarget::MasterMute { .. } => true,
        };
        for p in &mut cfg.midi.profiles {
            p.bindings
                .retain(|b| valid(&b.target) && !matches!(b.target, MidiTarget::SelectProfile { .. }));
        }
        cfg.midi.global_bindings.retain(|b| valid(&b.target));
        cfg
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, s);
        }
    }

    pub fn channel(&self, id: u64) -> Option<&ChannelConfig> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn channel_mut(&mut self, id: u64) -> Option<&mut ChannelConfig> {
        self.channels.iter_mut().find(|c| c.id == id)
    }

    /// Template names not yet used by an existing channel.
    pub fn unused_template_names(&self) -> Vec<&'static str> {
        TEMPLATE_NAMES
            .iter()
            .copied()
            .filter(|t| !self.channels.iter().any(|c| c.name == *t))
            .collect()
    }

    /// Add a channel (as a virtual device by default). Returns its id, or
    /// `None` when the channel limit is reached.
    pub fn add_channel(&mut self, name: Option<&str>) -> Option<u64> {
        if self.channels.len() >= MAX_CHANNELS {
            return None;
        }
        let id = self.next_channel_id;
        self.next_channel_id += 1;
        let name = name
            .map(str::to_string)
            .or_else(|| self.unused_template_names().first().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("Channel {id}"));
        self.channels.push(ChannelConfig {
            id,
            name,
            assignment: Some(Assignment::Virtual),
            // Muted towards the audience/recording until deliberately
            // unmuted, so a fresh channel never leaks audio into the
            // stream or VOD tracks.
            stream_muted: true,
            vod_muted: true,
            ..ChannelConfig::default()
        });
        Some(id)
    }

    pub fn remove_channel(&mut self, id: u64) {
        self.channels.retain(|c| c.id != id || c.permanent);
    }
}
