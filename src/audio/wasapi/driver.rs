//! The user-mode side of the Crossfade virtual audio driver (`driver/`).
//!
//! The driver exposes fixed endpoints:
//!
//! - two capture endpoints, "Crossfade Stream Mix" and "Crossfade VOD Mix",
//!   whose audio this app pushes in through a private KS property on their
//!   wave filters;
//! - `MAX_SLOTS` render endpoints, "Crossfade Channel 1…8", which discard
//!   what is played to them. The engine records them through ordinary
//!   WASAPI loopback, so they work like the per-channel null sinks on Linux.
//!
//! Keep the constants in sync with `driver/inc/crossfade.h`.

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_DRP_HARDWAREID, CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
    CM_GETIDLIST_FILTER_ENUMERATOR, CM_Get_DevNode_Registry_PropertyW, CM_Get_DevNode_Status,
    CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CM_PROB,
    CR_SUCCESS, DN_HAS_PROBLEM,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{GUID, PCWSTR};

/// Number of "Crossfade Channel N" render endpoints (= `MAX_CHANNELS`).
pub const MAX_SLOTS: usize = 8;

/// KSPROPSETID_Crossfade.
const PROPSET: GUID = GUID::from_u128(0x9c877548_3cf8_4138_88e2_c2d6dfb6b26b);
/// SET: interleaved stereo s32 frames at 48 kHz, appended to the ring.
/// Integer PCM so the kernel side never needs floating point.
const PROP_PUSH: u32 = 1;
/// Largest push the driver accepts, in frames (`CF_MAX_PUSH_FRAMES`, 100 ms).
const MAX_PUSH_FRAMES: usize = 4800;

const KSCATEGORY_AUDIO: GUID = GUID::from_u128(0x6994ad04_93ef_11d0_a3cc_00a0c9223196);
/// CTL_CODE(FILE_DEVICE_KS, 0x000, METHOD_NEITHER, FILE_ANY_ACCESS)
const IOCTL_KS_PROPERTY: u32 = 0x002f_0003;
const KSPROPERTY_TYPE_SET: u32 = 0x0000_0002;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DriverRole {
    StreamMic,
    VodMic,
    /// 0-based render slot.
    Channel(u8),
}

/// KS filter reference strings (lowercased; Windows lowercases interface
/// paths inconsistently, so all matching is case-insensitive).
const TOPO_STREAM: &str = "\\cftopostream";
const TOPO_VOD: &str = "\\cftopovod";
const TOPO_CHANNEL: &str = "\\cftopoch";
const WAVE_STREAM: &str = "\\cfwavestream";
const WAVE_VOD: &str = "\\cfwavevod";

/// Map the KS topology filter an endpoint is connected to back to its role.
pub fn role_from_filter_path(path: &str) -> Option<DriverRole> {
    let p = path.to_ascii_lowercase();
    if p.ends_with(TOPO_STREAM) {
        return Some(DriverRole::StreamMic);
    }
    if p.ends_with(TOPO_VOD) {
        return Some(DriverRole::VodMic);
    }
    let idx = p.rfind(TOPO_CHANNEL)?;
    let n: u8 = p[idx + TOPO_CHANNEL.len()..].parse().ok()?;
    (1..=MAX_SLOTS as u8).contains(&n).then(|| DriverRole::Channel(n - 1))
}

#[repr(C)]
struct KsProperty {
    set: GUID,
    id: u32,
    flags: u32,
}

/// Every present KSCATEGORY_AUDIO interface path.
fn audio_interfaces() -> Vec<String> {
    // SAFETY: size query followed by a fill of exactly that size.
    unsafe {
        let mut len = 0u32;
        if CM_Get_Device_Interface_List_SizeW(
            &mut len,
            &KSCATEGORY_AUDIO,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        ) != CR_SUCCESS
        {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_Interface_ListW(
            &KSCATEGORY_AUDIO,
            PCWSTR::null(),
            &mut buf,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        ) != CR_SUCCESS
        {
            return Vec::new();
        }
        buf.split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }
}

/// Hardware id of the root-enumerated device (`CF_HARDWARE_ID`).
const HARDWARE_ID: &str = r"ROOT\CrossfadeAudio";
/// CM_PROB_UNSIGNED_DRIVER: Windows refused an unsigned driver.
pub const PROBLEM_UNSIGNED: u32 = 52;

pub enum DriverState {
    /// The stream-mix filter is registered: the driver is running.
    Running,
    /// The device exists but Windows reports a problem (Device Manager code).
    Problem(u32),
    /// No Crossfade device on this system.
    Missing,
}

/// Is the driver installed and running, and if not, why?
pub fn state() -> DriverState {
    if audio_interfaces()
        .iter()
        .any(|p| p.to_ascii_lowercase().ends_with(WAVE_STREAM))
    {
        return DriverState::Running;
    }
    device_problem().map_or(DriverState::Missing, DriverState::Problem)
}

/// Problem code of our device node (0 = present without a problem), or
/// `None` when no such device exists.
fn device_problem() -> Option<u32> {
    let filter = windows::core::HSTRING::from("ROOT");
    // SAFETY: size query followed by a fill of exactly that size; property
    // reads into correctly sized buffers.
    unsafe {
        let mut len = 0u32;
        if CM_Get_Device_ID_List_SizeW(&mut len, &filter, CM_GETIDLIST_FILTER_ENUMERATOR)
            != CR_SUCCESS
        {
            return None;
        }
        let mut ids = vec![0u16; len as usize];
        if CM_Get_Device_ID_ListW(&filter, &mut ids, CM_GETIDLIST_FILTER_ENUMERATOR) != CR_SUCCESS {
            return None;
        }
        for id in ids.split(|&c| c == 0).filter(|s| !s.is_empty()) {
            let id: Vec<u16> = id.iter().copied().chain(std::iter::once(0)).collect();
            let mut devinst = 0u32;
            if CM_Locate_DevNodeW(&mut devinst, PCWSTR(id.as_ptr()), CM_LOCATE_DEVNODE_NORMAL)
                != CR_SUCCESS
            {
                continue;
            }
            let mut buf = [0u16; 512];
            let mut size = std::mem::size_of_val(&buf) as u32;
            if CM_Get_DevNode_Registry_PropertyW(
                devinst,
                CM_DRP_HARDWAREID,
                None,
                Some(buf.as_mut_ptr().cast()),
                &mut size,
                0,
            ) != CR_SUCCESS
            {
                continue;
            }
            let ours = buf
                .split(|&c| c == 0)
                .any(|h| String::from_utf16_lossy(h).eq_ignore_ascii_case(HARDWARE_ID));
            if !ours {
                continue;
            }
            let mut status = CM_DEVNODE_STATUS_FLAGS(0);
            let mut problem = CM_PROB(0);
            if CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) != CR_SUCCESS {
                return Some(0);
            }
            return Some(if status.0 & DN_HAS_PROBLEM.0 != 0 { problem.0 } else { 0 });
        }
        None
    }
}

/// A write handle on one virtual microphone's wave filter.
pub struct MicFeed {
    handle: HANDLE,
    /// Conversion buffer, reused across pushes.
    pcm: Vec<i32>,
}

// SAFETY: a file handle; DeviceIoControl may be called from any thread.
unsafe impl Send for MicFeed {}

impl MicFeed {
    pub fn open(role: DriverRole) -> Option<Self> {
        let suffix = match role {
            DriverRole::StreamMic => WAVE_STREAM,
            DriverRole::VodMic => WAVE_VOD,
            DriverRole::Channel(_) => return None,
        };
        let path = audio_interfaces()
            .into_iter()
            .find(|p| p.to_ascii_lowercase().ends_with(suffix))?;
        let wide = windows::core::HSTRING::from(path);
        // SAFETY: opening a KS filter by its interface path.
        let handle = unsafe {
            CreateFileW(
                &wide,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .ok()?
        };
        Some(Self {
            handle,
            pcm: Vec::new(),
        })
    }

    /// Append interleaved stereo frames to the microphone's ring buffer, in
    /// pieces no larger than the driver accepts. Returns false when the
    /// driver rejected a call (e.g. it was unloaded), so the caller can
    /// reopen.
    pub fn push(&mut self, frames: &[f32]) -> bool {
        frames
            .chunks(MAX_PUSH_FRAMES * 2)
            .all(|chunk| self.push_chunk(chunk))
    }

    fn push_chunk(&mut self, frames: &[f32]) -> bool {
        self.pcm.clear();
        self.pcm
            .extend(frames.iter().map(|s| (s.clamp(-1.0, 1.0) * i32::MAX as f32) as i32));
        let prop = KsProperty {
            set: PROPSET,
            id: PROP_PUSH,
            flags: KSPROPERTY_TYPE_SET,
        };
        let mut returned = 0u32;
        // SAFETY: both buffers outlive the synchronous call. For KS
        // property sets the value travels in the "output" buffer.
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_KS_PROPERTY,
                Some(&prop as *const KsProperty as *const _),
                std::mem::size_of::<KsProperty>() as u32,
                Some(self.pcm.as_ptr() as *mut _),
                std::mem::size_of_val(self.pcm.as_slice()) as u32,
                Some(&mut returned),
                None,
            )
            .is_ok()
        }
    }
}

impl Drop for MicFeed {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}




