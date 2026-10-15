//! Playback session discovery: which processes currently own audio
//! sessions, and under which application name. The name is what App
//! channel rules match against (the executable's stem, e.g. "Spotify",
//! "firefox", "Discord").

use std::collections::{BTreeMap, HashMap};

use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::{Interface, PWSTR};

pub struct Sessions {
    enumerator: IMMDeviceEnumerator,
    /// pid → app name; process names are cached since pids are only reused
    /// after the process is gone (and its sessions with it).
    names: HashMap<u32, Option<String>>,
}

impl Sessions {
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: standard MMDevice activation on a COM-initialized thread.
        let enumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        Ok(Self {
            enumerator,
            names: HashMap::new(),
        })
    }

    /// Every process with a live playback session on any render endpoint,
    /// as pid → application name. Our own process and the system sounds
    /// session are excluded.
    pub fn scan(&mut self) -> BTreeMap<u32, String> {
        let own = std::process::id();
        let mut out = BTreeMap::new();
        // SAFETY: COM enumeration on the thread that created the enumerator.
        unsafe {
            let Ok(devices) = self.enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            else {
                return out;
            };
            for i in 0..devices.GetCount().unwrap_or(0) {
                let Ok(device) = devices.Item(i) else {
                    continue;
                };
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)
                else {
                    continue;
                };
                let Ok(list) = manager.GetSessionEnumerator() else {
                    continue;
                };
                for j in 0..list.GetCount().unwrap_or(0) {
                    let Ok(control) = list.GetSession(j) else {
                        continue;
                    };
                    let Ok(control) = control.cast::<IAudioSessionControl2>() else {
                        continue;
                    };
                    if control.IsSystemSoundsSession().0 == 0 {
                        continue;
                    }
                    if control
                        .GetState()
                        .is_ok_and(|s| s == AudioSessionStateExpired)
                    {
                        continue;
                    }
                    let Ok(pid) = control.GetProcessId() else {
                        continue;
                    };
                    if pid == 0 || pid == own {
                        continue;
                    }
                    let name = self
                        .names
                        .entry(pid)
                        .or_insert_with(|| process_name(pid))
                        .clone();
                    if let Some(name) = name {
                        out.insert(pid, name);
                    }
                }
            }
        }
        self.names.retain(|pid, _| out.contains_key(pid));
        out
    }
}

/// Executable stem of a process ("C:\…\Spotify.exe" → "Spotify").
fn process_name(pid: u32) -> Option<String> {
    // SAFETY: limited-information handle, closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; MAX_PATH as usize * 2];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
    }
}
