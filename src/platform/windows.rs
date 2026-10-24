//! Windows integration: launch at login through the per-user `Run` key.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, REG_SZ,
};
use windows::core::HSTRING;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "Gleem Crossfade";

fn io_err(e: WIN32_ERROR) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.0 as i32)
}

pub fn autostart_enabled() -> bool {
    let key = HSTRING::from(RUN_KEY);
    let name = HSTRING::from(VALUE_NAME);
    let mut size = 0u32;
    // SAFETY: size query only; no output buffer.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &key,
            &name,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    r.is_ok()
}

/// Enable/disable launch-on-login. The entry runs the current executable
/// with --hidden so the virtual devices come up in the background without
/// opening the window.
pub fn set_autostart(enable: bool) -> std::io::Result<()> {
    let key = HSTRING::from(RUN_KEY);
    let name = HSTRING::from(VALUE_NAME);
    if !enable {
        // SAFETY: plain registry call with valid wide strings.
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) };
        return match r {
            e if e == ERROR_FILE_NOT_FOUND => Ok(()),
            e if e.is_ok() => Ok(()),
            e => Err(io_err(e)),
        };
    }
    let exe = std::env::current_exe()?;
    let command = format!("\"{}\" --hidden", exe.display());
    let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer of the given size.
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &key,
            &name,
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        )
    };
    if r.is_ok() { Ok(()) } else { Err(io_err(r)) }
}

