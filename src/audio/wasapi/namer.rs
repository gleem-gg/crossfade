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
    let clean: String = channel_name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return String::new();
    }
    format!("Crossfade: {clean}").chars().take(MAX_NAME).collect()
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
