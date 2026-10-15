//! Audio routing. The platform backend is chosen at compile time; both
//! expose the same `AudioManager` API to the UI:
//!
//! - Linux (`pulse.rs`): PipeWire through the PulseAudio client API, running
//!   entirely on the GTK main loop.
//! - Windows (`wasapi/`): a WASAPI mixing engine on a real-time thread,
//!   feeding the Crossfade virtual audio driver (`driver/`).

#[cfg(target_os = "linux")]
mod pulse;
#[cfg(target_os = "linux")]
pub use pulse::PulseManager as AudioManager;

#[cfg(windows)]
mod wasapi;
#[cfg(windows)]
pub use wasapi::WasapiManager as AudioManager;

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
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    VstChanged(u64),
    /// VST parameters were edited from a plugin's own window and written
    /// into the config: (channel, [(plugin cfg_id, param index, value)]).
    /// The config should be persisted and open dialog sliders updated.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    VstParams(u64, Vec<(u64, u32, f64)>),
}

#[derive(Clone, Debug)]
pub struct SourceEntry {
    pub name: String,
    pub description: String,
    pub is_monitor: bool,
    /// Current device volume in percent (avg across channels).
    pub volume: f64,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub channels: u8,
    /// Server-side index, for matching capture streams to their device.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub index: u32,
    /// Owning sound card, for same-card checks against sinks.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub card: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct SinkEntry {
    pub name: String,
    pub description: String,
    /// Current device volume in percent (avg across channels).
    pub volume: f64,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub channels: u8,
    /// Owning sound card, for same-card checks against sources.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub card: Option<u32>,
}

/// Does this device name/description belong to an Elgato Wave XLR?
pub(crate) fn is_wave_xlr(name: &str, description: &str) -> bool {
    let n = name.to_ascii_lowercase();
    description.contains("Wave XLR") || n.contains("wave_xlr") || n.contains("wave-xlr")
}
