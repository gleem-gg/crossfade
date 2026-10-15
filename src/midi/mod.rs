//! MIDI controller input/output. The platform backend exposes the same
//! `MidiManager` API to the UI:
//!
//! - Linux (`alsa_seq.rs`): a duplex ALSA sequencer client, PipeWire-bridged.
//! - Windows (`winmm.rs`): the WinMM MIDI API, with polling-based hotplug.
//!
//! Controllers are identified by *name* (the USB product string), which is
//! stable across replugs; backend handles are only runtime map keys.

#[cfg(target_os = "linux")]
mod alsa_seq;
#[cfg(target_os = "linux")]
pub use alsa_seq::MidiManager;

#[cfg(windows)]
mod winmm;
#[cfg(windows)]
pub use winmm::MidiManager;

#[derive(Clone, Debug)]
pub enum MidiEvent {
    /// A control-change message (CC) from a connected controller.
    Control {
        device: String,
        channel: u8,
        number: u8,
        value: u8,
    },
    /// A note-on with velocity > 0 (pad/button press). Note-offs and
    /// zero-velocity note-ons are filtered out at this layer.
    NoteOn {
        device: String,
        channel: u8,
        number: u8,
    },
    /// A controller appeared or disappeared.
    DevicesChanged,
}

/// Velocity → 0xRRGGBB palette shared by the common RGB pad surfaces
/// (Novation Launchpads; Akai adopted the same table for the APC mini mk2).
/// Mono-LED controllers simply light up on any non-zero velocity, so using
/// this table universally degrades gracefully.
const PAD_PALETTE: [u32; 128] = [
    0x000000, 0x1E1E1E, 0x7F7F7F, 0xFFFFFF, 0xFF4C4C, 0xFF0000, 0x590000, 0x190000,
    0xFFBD6C, 0xFF5400, 0x591D00, 0x271B00, 0xFFFF4C, 0xFFFF00, 0x595900, 0x191900,
    0x88FF4C, 0x54FF00, 0x1D5900, 0x142B00, 0x4CFF4C, 0x00FF00, 0x005900, 0x001900,
    0x4CFF5E, 0x00FF19, 0x00590D, 0x001902, 0x4CFF88, 0x00FF55, 0x00591D, 0x001F12,
    0x4CFFB7, 0x00FF99, 0x005935, 0x001912, 0x4CC3FF, 0x00A9FF, 0x004152, 0x001019,
    0x4C88FF, 0x0055FF, 0x001D59, 0x000819, 0x4C4CFF, 0x0000FF, 0x000059, 0x000019,
    0x874CFF, 0x5400FF, 0x190064, 0x0F0030, 0xFF4CFF, 0xFF00FF, 0x590059, 0x190019,
    0xFF4C87, 0xFF0054, 0x59001D, 0x220013, 0xFF1500, 0x993500, 0x795100, 0x436400,
    0x033900, 0x005735, 0x00547F, 0x0000FF, 0x00454F, 0x2500CC, 0x7F7F7F, 0x202020,
    0xFF0000, 0xBDFF2D, 0xAFED06, 0x64FF09, 0x108B00, 0x00FF87, 0x00A9FF, 0x002AFF,
    0x3F00FF, 0x7A00FF, 0xB21A7D, 0x402100, 0xFF4A00, 0x88E106, 0x72FF15, 0x00FF00,
    0x3BFF26, 0x59FF71, 0x38FFCC, 0x5B8AFF, 0x3151C6, 0x877FE9, 0xD31DFF, 0xFF005D,
    0xFF7F00, 0xB9B000, 0x90FF00, 0x835D07, 0x392B00, 0x144C10, 0x0D5038, 0x15152A,
    0x16205A, 0x693C1C, 0xA8000A, 0xDE513D, 0xD86A1C, 0xFFE126, 0x9EE12F, 0x67B50F,
    0x1E1E30, 0xDCFF6B, 0x80FFBD, 0x9A99FF, 0x8E66FF, 0x404040, 0x757575, 0xE0FFFF,
    0xA00000, 0x350000, 0x1AD000, 0x074200, 0xB9B000, 0x3F3100, 0xB35F00, 0x4B1502,
];

/// Velocity whose palette entry is nearest (RGB distance) to the requested
/// color — how a channel color is shown on a pad bound as its color pad.
pub fn color_velocity(r: u8, g: u8, b: u8) -> u8 {
    let mut best = 0u8;
    let mut best_d = i32::MAX;
    for (i, &c) in PAD_PALETTE.iter().enumerate() {
        let dr = ((c >> 16) & 0xFF) as i32 - r as i32;
        let dg = ((c >> 8) & 0xFF) as i32 - g as i32;
        let db = (c & 0xFF) as i32 - b as i32;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}

/// Per-device LED quirks. Pads are normally lit by echoing the note on the
/// channel the binding listens on, but on the APC mini mk2 the note-on
/// channel selects pad *brightness* (0 = 10% … 6 = 100%, 7+ = pulse/blink),
/// so echoing on the learned channel 0 comes out barely visible — use full
/// brightness instead.
pub(crate) fn led_channel(device: &str, bound: u8) -> u8 {
    if device.eq_ignore_ascii_case("APC mini mk2") {
        6
    } else {
        bound
    }
}

