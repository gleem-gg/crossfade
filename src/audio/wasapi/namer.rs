//! Client of the endpoint naming service (`driver/setup/service.cpp`).
//!
//! Renaming an audio endpoint needs administrator rights, so the app asks
//! a small LocalSystem service, installed with the driver, to name the
//! "Crossfade Channel N" outputs after their channels — "Crossfade: Game",
//! like the channel sinks on Linux. Without the service nothing happens and
//! the outputs keep their numbered names.

use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;

use windows::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

/// Keep in sync with `CF_NAMER_PIPE` / `CF_NAMER_MAX_NAME` in
/// `driver/inc/crossfade.h`.
const PIPE: &str = r"\\.\pipe\GleemCrossfadeEndpointNames";
const MAX_NAME: usize = 64;

/// The description a channel's output should carry ("Crossfade: Game").
pub fn channel_endpoint_name(channel_name: &str) -> String {
    let name = clean_name(channel_name);
    if name.is_empty() {
        return String::new();
    }
    clean_name(&format!("Crossfade: {name}"))
}

/// Characters names must not contain: controls and bidi formatting
/// characters, which could make a device name display as something else
/// (e.g. U+202E reverses the text after it).
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// The service's `CleanName` (`driver/setup/service.cpp`), exactly: drop
/// unsafe characters, trim spaces, cap at `MAX_NAME` UTF-16 units without
/// splitting a character. The app compares Windows' names against this, so
/// any difference would make it re-request names forever.
fn clean_name(name: &str) -> String {
    let kept: String = name.chars().filter(|&c| !is_unsafe(c)).collect();
    let mut out = String::new();
    let mut units = 0;
    for c in kept.trim_matches(' ').chars() {
        units += c.len_utf16();
        if units > MAX_NAME {
            break;
        }
        out.push(c);
    }
    out.trim_end_matches(' ').to_string()
}

/// Ask the service to apply `names` (1-based slot, description; empty =
/// the default "Crossfade Channel N"). Runs on a short-lived thread so a
/// busy or missing service never stalls the UI.
pub fn request(names: Vec<(u8, String)>) {
    std::thread::spawn(move || {
        // Identification only: whoever serves the pipe may learn who we
        // are but cannot act as us, should something else own the name.
        let Ok(mut pipe) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .open(PIPE)
        else {
            return;
        };
        let message: String = names
            .iter()
            .map(|(slot, name)| format!("{slot}\t{name}\n"))
            .collect();
        if pipe.write_all(message.as_bytes()).is_ok() {
            let mut reply = [0u8; 16];
            let _ = pipe.read(&mut reply);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{MAX_NAME, channel_endpoint_name};

    #[test]
    fn names_drop_controls_and_bidi_overrides() {
        assert_eq!(channel_endpoint_name("  Game  "), "Crossfade: Game");
        assert_eq!(channel_endpoint_name("Ga\tme\u{7}"), "Crossfade: Game");
        assert_eq!(channel_endpoint_name("evil\u{202E}txt.exe"), "Crossfade: eviltxt.exe");
        assert_eq!(channel_endpoint_name("\u{2066}Music\u{2069}"), "Crossfade: Music");
        assert_eq!(channel_endpoint_name(" \u{202E} "), "");
    }

    #[test]
    fn names_are_capped_without_splitting_characters() {
        // "Crossfade: " is 11 units; an emoji is 2 UTF-16 units.
        let long = format!("{}🎮🎮", "x".repeat(MAX_NAME - 11 - 1));
        let name = channel_endpoint_name(&long);
        assert_eq!(name.encode_utf16().count(), MAX_NAME - 1);
        assert!(name.ends_with('x'));

        // A cut that lands on a space leaves no trailing space behind.
        let spaced = format!("{} tail", "y".repeat(MAX_NAME - 11));
        assert_eq!(channel_endpoint_name(&spaced).encode_utf16().count(), MAX_NAME);
        let spaced = format!("{} tail", "y".repeat(MAX_NAME - 12));
        assert!(!channel_endpoint_name(&spaced).ends_with(' '));
    }
}
