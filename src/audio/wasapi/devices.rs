//! Endpoint enumeration and per-device controls through the MMDevice API,
//! on the main thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IDeviceTopology, IMMDevice,
    IMMDeviceEnumerator, IMMEndpoint, IMMNotificationClient, IMMNotificationClient_Impl,
    MMDeviceEnumerator, eAll, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::core::{PCWSTR, PWSTR, implement};

use super::driver::{self, DriverRole};

#[derive(Clone, Debug)]
pub struct Endpoint {
    /// MMDevice id (`{0.0.0.00000000}.{guid}`).
    pub id: String,
    pub name: String,
    pub is_render: bool,
    pub channels: u8,
    /// Device volume in percent.
    pub volume: f64,
    /// Set for the endpoints of the Crossfade virtual audio driver.
    pub role: Option<DriverRole>,
}

pub struct Devices {
    enumerator: IMMDeviceEnumerator,
    /// Raised from the notification thread on any endpoint change.
    dirty: Arc<AtomicBool>,
    _client: IMMNotificationClient,
}

/// Owned string out of a COM-allocated wide string.
fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: COM allocated, NUL-terminated; freed right after copying.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

impl Devices {
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: standard MMDevice activation on a COM-initialized thread.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dirty = Arc::new(AtomicBool::new(true));
            let client: IMMNotificationClient = Notifier {
                dirty: dirty.clone(),
            }
            .into();
            enumerator.RegisterEndpointNotificationCallback(&client)?;
            Ok(Self {
                enumerator,
                dirty,
                _client: client,
            })
        }
    }

    /// True (once) after an endpoint was added, removed or changed.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    pub fn enumerate(&self) -> Vec<Endpoint> {
        let mut out = Vec::new();
        // SAFETY: COM calls on the thread that created the enumerator.
        unsafe {
            let Ok(collection) = self.enumerator.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)
            else {
                return out;
            };
            let count = collection.GetCount().unwrap_or(0);
            for i in 0..count {
                let Ok(device) = collection.Item(i) else {
                    continue;
                };
                if let Some(ep) = describe(&device) {
                    out.push(ep);
                }
            }
        }
        out
    }

    pub fn default_endpoint(&self, render: bool) -> Option<String> {
        let flow = if render { eRender } else { eCapture };
        // SAFETY: plain COM getters.
        unsafe {
            let device = self
                .enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .ok()?;
            Some(take_pwstr(device.GetId().ok()?))
        }
    }

    pub fn set_volume(&self, id: &str, percent: f64) {
        let wide = windows::core::HSTRING::from(id);
        // SAFETY: plain COM calls.
        unsafe {
            let Ok(device) = self.enumerator.GetDevice(PCWSTR(wide.as_ptr())) else {
                return;
            };
            let Ok(vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) else {
                return;
            };
            let _ = vol.SetMasterVolumeLevelScalar(
                (percent / 100.0).clamp(0.0, 1.0) as f32,
                std::ptr::null(),
            );
        }
    }
}

/// Read everything the UI and router need about one endpoint.
unsafe fn describe(device: &IMMDevice) -> Option<Endpoint> {
    // SAFETY: caller passes a live device; all calls are plain getters.
    unsafe {
        let id = take_pwstr(device.GetId().ok()?);
        let endpoint: IMMEndpoint = windows::core::Interface::cast(device).ok()?;
        let is_render = endpoint.GetDataFlow().ok()? == eRender;
        let name = string_property(device, &PKEY_Device_FriendlyName).unwrap_or_else(|| id.clone());
        let (channels, volume) = match device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            Ok(v) => (
                v.GetChannelCount().unwrap_or(2).clamp(1, 255) as u8,
                f64::from(v.GetMasterVolumeLevelScalar().unwrap_or(1.0)) * 100.0,
            ),
            Err(_) => (2, 100.0),
        };
        Some(Endpoint {
            role: driver_role(device),
            id,
            name,
            is_render,
            channels,
            volume,
        })
    }
}

unsafe fn string_property(device: &IMMDevice, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: property store read of a well-known key.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(key).ok()?;
        let s = PropVariantToStringAlloc(&value).ok()?;
        Some(take_pwstr(s))
    }
}

/// Endpoints of the Crossfade driver are recognized by the KS topology
/// filter they hang off (its reference string), not by their name, which
/// the user can change in the Sound settings.
unsafe fn driver_role(device: &IMMDevice) -> Option<DriverRole> {
    // SAFETY: device topology walk; every step is a getter.
    unsafe {
        let topology = device.Activate::<IDeviceTopology>(CLSCTX_ALL, None).ok()?;
        let connector = topology.GetConnector(0).ok()?;
        let filter = take_pwstr(connector.GetDeviceIdConnectedTo().ok()?);
        driver::role_from_filter_path(&filter)
    }
}

/// Endpoint notifications arrive on an MTA worker thread; they only flag
/// the main thread, which re-enumerates on its next poll.
#[implement(IMMNotificationClient)]
struct Notifier {
    dirty: Arc<AtomicBool>,
}

impl IMMNotificationClient_Impl for Notifier_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        _flow: EDataFlow,
        _role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        // Fires for volume changes too, which the sidebar shows.
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }
}
