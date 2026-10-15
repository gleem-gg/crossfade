//! Undocumented Windows audio policy interfaces — the same ones the Sound
//! settings page and EarTrumpet use:
//!
//! - `IPolicyConfig` (CPolicyConfigClient) sets the system default endpoint.
//! - `IAudioPolicyConfigFactory` (`Windows.Media.Internal.AudioPolicyConfig`)
//!   sets a *per-process* default endpoint ("App volume and device
//!   preferences"), which is how App/catch-all channels claim playback.
//!
//! Both are best-effort: if an interface cannot be obtained (very old or
//! future builds), the corresponding feature silently does nothing.

#![allow(non_snake_case)]

use std::ffi::c_void;

use windows::Win32::Media::Audio::{eCommunications, eConsole, eMultimedia, EDataFlow, ERole};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::WinRT::RoGetActivationFactory;
use windows::core::{
    GUID, HRESULT, HSTRING, IInspectable, IUnknown,
    IUnknown_Vtbl, Interface, PCWSTR,
};

#[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    fn GetMixFormat(&self, id: PCWSTR, format: *mut *mut c_void) -> HRESULT;
    fn GetDeviceFormat(&self, id: PCWSTR, default: i32, format: *mut *mut c_void) -> HRESULT;
    fn ResetDeviceFormat(&self, id: PCWSTR) -> HRESULT;
    fn SetDeviceFormat(&self, id: PCWSTR, endpoint: *mut c_void, mix: *mut c_void) -> HRESULT;
    fn GetProcessingPeriod(
        &self,
        id: PCWSTR,
        default: i32,
        default_period: *mut i64,
        min_period: *mut i64,
    ) -> HRESULT;
    fn SetProcessingPeriod(&self, id: PCWSTR, period: *mut i64) -> HRESULT;
    fn GetShareMode(&self, id: PCWSTR, mode: *mut c_void) -> HRESULT;
    fn SetShareMode(&self, id: PCWSTR, mode: *mut c_void) -> HRESULT;
    fn GetPropertyValue(&self, id: PCWSTR, key: *const c_void, value: *mut c_void) -> HRESULT;
    fn SetPropertyValue(&self, id: PCWSTR, key: *const c_void, value: *mut c_void) -> HRESULT;
    fn SetDefaultEndpoint(&self, id: PCWSTR, role: ERole) -> HRESULT;
    fn SetEndpointVisibility(&self, id: PCWSTR, visible: i32) -> HRESULT;
}

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// Make an endpoint the system default for every role (console, multimedia
/// and communications), like picking it in the Sound settings.
pub fn set_default_endpoint(id: &str) -> windows::core::Result<()> {
    let id = HSTRING::from(id);
    // SAFETY: COM is initialized on the calling (main) thread.
    unsafe {
        let pc: IPolicyConfig = CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        for role in [eConsole, eMultimedia, eCommunications] {
            pc.SetDefaultEndpoint(PCWSTR(id.as_ptr()), role).ok()?;
        }
    }
    Ok(())
}

/// Per-process endpoint routing. An IInspectable-derived interface whose
/// vtable starts with the three IInspectable methods and 19 more this app
/// does not use (volume groups, chat apps, …); only their count matters.
#[windows::core::interface("ab3d4648-e242-459f-b02f-541c70306324")]
unsafe trait IAudioPolicyConfigFactory: IUnknown {
    fn _GetIids(&self) -> HRESULT;
    fn _GetRuntimeClassName(&self) -> HRESULT;
    fn _GetTrustLevel(&self) -> HRESULT;
    fn _r0(&self) -> HRESULT;
    fn _r1(&self) -> HRESULT;
    fn _r2(&self) -> HRESULT;
    fn _r3(&self) -> HRESULT;
    fn _r4(&self) -> HRESULT;
    fn _r5(&self) -> HRESULT;
    fn _r6(&self) -> HRESULT;
    fn _r7(&self) -> HRESULT;
    fn _r8(&self) -> HRESULT;
    fn _r9(&self) -> HRESULT;
    fn _r10(&self) -> HRESULT;
    fn _r11(&self) -> HRESULT;
    fn _r12(&self) -> HRESULT;
    fn _r13(&self) -> HRESULT;
    fn _r14(&self) -> HRESULT;
    fn _r15(&self) -> HRESULT;
    fn _r16(&self) -> HRESULT;
    fn _r17(&self) -> HRESULT;
    fn _r18(&self) -> HRESULT;
    fn SetPersistedDefaultAudioEndpoint(
        &self,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: *mut c_void,
    ) -> HRESULT;
    fn GetPersistedDefaultAudioEndpoint(
        &self,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: *mut HSTRING,
    ) -> HRESULT;
    fn ClearAllPersistedApplicationDefaultEndpoints(&self) -> HRESULT;
}

/// Pre-21H2 builds expose the same vtable under a different IID.
const IID_AUDIO_POLICY_CONFIG_FACTORY_DOWNLEVEL: GUID =
    GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

/// The per-app policy uses full device-interface paths, not MMDevice ids.
const MMDEVAPI_TOKEN: &str = r"\\?\SWD#MMDEVAPI#";
const DEVINTERFACE_AUDIO_RENDER: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";

pub struct AppRouting {
    factory: IAudioPolicyConfigFactory,
}

impl AppRouting {
    pub fn new() -> Option<Self> {
        let class = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
        // SAFETY: plain activation-factory lookups; the downlevel interface
        // shares the vtable layout, so reinterpreting it is sound.
        unsafe {
            if let Ok(factory) = RoGetActivationFactory::<IAudioPolicyConfigFactory>(&class) {
                return Some(Self { factory });
            }
            let mut raw: *mut c_void = std::ptr::null_mut();
            let unknown: IInspectable =
                RoGetActivationFactory::<IInspectable>(&class).ok()?;
            unknown
                .query(&IID_AUDIO_POLICY_CONFIG_FACTORY_DOWNLEVEL, &mut raw)
                .ok()
                .ok()?;
            Some(Self {
                factory: IAudioPolicyConfigFactory::from_raw(raw),
            })
        }
    }

    /// The render endpoint (MMDevice id) a process is pinned to, if any.
    pub fn get(&self, pid: u32) -> Option<String> {
        let mut out = HSTRING::new();
        // SAFETY: `out` receives an owned HSTRING.
        let hr = unsafe {
            self.factory.GetPersistedDefaultAudioEndpoint(
                pid,
                windows::Win32::Media::Audio::eRender,
                eMultimedia,
                &mut out,
            )
        };
        if hr.is_err() || out.is_empty() {
            return None;
        }
        let full = out.to_string_lossy();
        Some(
            full.strip_prefix(MMDEVAPI_TOKEN)
                .and_then(|s| s.strip_suffix(DEVINTERFACE_AUDIO_RENDER))
                .unwrap_or(&full)
                .to_string(),
        )
    }

    /// Pin a process's playback to an endpoint (`None` = follow the system
    /// default again).
    pub fn set(&self, pid: u32, endpoint: Option<&str>) -> bool {
        let full = endpoint
            .map(|id| HSTRING::from(format!("{MMDEVAPI_TOKEN}{id}{DEVINTERFACE_AUDIO_RENDER}")))
            .unwrap_or_default();
        let mut ok = true;
        for role in [eConsole, eMultimedia] {
            // SAFETY: the HSTRING is borrowed for the duration of the call
            // (the callee duplicates it); a null HSTRING clears the entry.
            let hr = unsafe {
                let raw: *mut c_void = std::mem::transmute_copy(&full);
                self.factory.SetPersistedDefaultAudioEndpoint(
                    pid,
                    windows::Win32::Media::Audio::eRender,
                    role,
                    raw,
                )
            };
            ok &= hr.is_ok();
        }
        ok
    }
}

